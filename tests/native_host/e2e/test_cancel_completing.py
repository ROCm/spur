# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for cancel and preempt-cancel routing through COMPLETING."""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

# SIG_IGN for TERM is inherited by every child, so only the agent's SIGKILL
# ends this run — the COMPLETING window is the agent's full grace period.
_STUBBORN_SCRIPT = "#!/bin/bash\ntrap '' TERM\nsleep 8675309\n"
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

# A job can leave RUNNING between the state check and the kill (a first-dispatch
# hiccup re-pends it), which leaves nothing to observe. Re-stage instead.
_STAGE_ATTEMPTS = 3


def _show_field(cluster, job_id: int, field: str) -> str:
    out = cluster.scontrol("show", "job", str(job_id))
    for token in out.split():
        if token.startswith(f"{field}="):
            return token.split("=", 1)[1]
    return ""


def _running_with_nodes(cluster, job_id: int) -> bool:
    return (
        job_state(cluster.squeue_all(), job_id) == "R"
        and _show_field(cluster, job_id, "NodeList") not in ("", "(null)")
    )


def _watch_states(cluster, job_id: int, samples: int = 400) -> list[str]:
    """Sample the job's state on the controller node until it goes terminal.

    One SSH round trip for the whole loop: polling from the test host costs
    100ms-2s per sample, which is the same order as the window being measured.
    """
    env = " ".join(cluster._cli_env_assignments())
    squeue = f"{env} {cluster.bin_dir}/squeue"
    out = cluster.nodes[0].exec_allow_fail(
        f"for i in $(seq 1 {samples}); do "
        f"s=$({squeue} -t all -h -j {job_id} -o '%t' 2>/dev/null | tr -d ' '); "
        'echo "${s:-NONE}"; '
        'case "$s" in CA|F|CD|TO|NF) break;; esac; sleep 0.05; done'
    )
    seq = []
    for line in out.strip().splitlines():
        state = line.strip()
        if state and (not seq or seq[-1] != state):
            seq.append(state)
    return seq


def _kill_and_watch(cluster, stage, kill) -> tuple[int, list[str]]:
    """Stage a victim, kill it, and return (job_id, observed state sequence).

    `stage` returns a running job id; `kill` takes it terminal. Retried while
    the victim races out of RUNNING before the kill lands.
    """
    last = None
    for _ in range(_STAGE_ATTEMPTS):
        job_id = stage()
        wait_job_state(cluster, job_id, "R", timeout=60)
        if not _running_with_nodes(cluster, job_id):
            cluster.cli_allow_fail(["scancel", str(job_id)])
            continue
        kill(job_id)
        seq = _watch_states(cluster, job_id)
        if "CA" in seq or "CG" in seq:
            return job_id, seq
        last = (job_id, seq)
        cluster.cli_allow_fail(["scancel", str(job_id)])
    if last:
        return last
    raise AssertionError("could not stage a running victim to cancel")


def _assert_completing_then_cancelled(job_id: int, seq: list[str]) -> None:
    assert "CG" in seq, (
        f"job {job_id} went {'->'.join(seq)} — it never waited in COMPLETING, so "
        "the allocation was freed before the node reported its release"
    )
    assert seq[-1] == "CA", (
        f"job {job_id} went {'->'.join(seq)} — a cancelled run must finalize as "
        "CANCELLED, not as the ordinary signal death its exit code describes"
    )


class TestCancelHoldsAllocationUntilRelease:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_AUTH_ROOT, "partitions": [_PARTITION]}

    def test_cancel_then_redispatch_onto_the_same_node(self, cluster):
        node = cluster.node_names[0]
        victim = cluster.write_file("stubborn.sh", _STUBBORN_SCRIPT)
        contender_script = cluster.write_file("contender.sh", _QUICK_SCRIPT)
        contender = []

        def stage():
            return parse_job_id(
                cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
            )

        def kill(job_id):
            # Queued before the cancel so it is ready the instant the node frees —
            # nothing stands between the two but one scheduler tick.
            cid = parse_job_id(
                cluster.sbatch(
                    ["-N1", "--exclusive", f"--nodelist={node}", contender_script]
                )
            )
            wait_job_state(cluster, cid, "PD", timeout=60)
            contender.append(cid)
            cluster.scancel(job_id)

        try:
            victim_id, seq = _kill_and_watch(cluster, stage, kill)
            _assert_completing_then_cancelled(victim_id, seq)

            contender_id = contender[-1]
            final = wait_job(cluster, contender_id, timeout=120)
            assert final == "CD", (
                f"contender must start once the node is released; got {final!r}"
            )
            # The regression signal: a hard rejection shows up as a retry, not a
            # failure, because the dispatch abort re-pends the job.
            restarts = _show_field(cluster, contender_id, "Restarts")
            assert restarts in ("", "0"), (
                f"contender was re-dispatched {restarts} time(s) — it was rejected "
                "for an allocation mismatch before it finally ran"
            )
        finally:
            for jid in contender:
                cluster.cli_allow_fail(["scancel", str(jid)])

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

        def stage():
            return parse_job_id(
                cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", script])
            )

        def kill(job_id):
            cluster.scancel(job_id)
            cluster.scontrol(
                "update", f"NodeName={node}", "State=DOWN", "Reason=e2e-cancel-evict"
            )

        try:
            job_id, seq = _kill_and_watch(cluster, stage, kill)
            assert seq[-1] == "CA", (
                f"job {job_id} went {'->'.join(seq)} — evicting a cancelled job as "
                "NODE_FAIL would also requeue a job the user had already cancelled"
            )
            # It must stay dead: NODE_FAIL is a requeue trigger, CANCELLED is not.
            time.sleep(10)
            assert job_state(cluster.squeue_all(), job_id) not in ("PD", "R"), (
                "a cancelled job was resurrected by the eviction path"
            )
        finally:
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
        aggressor_script = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)
        aggressor = []

        def stage():
            return parse_job_id(
                cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
            )

        def kill(_job_id):
            aid = parse_job_id(
                cluster.sbatch(
                    ["-N1", "--exclusive", f"--nodelist={node}", aggressor_script]
                )
            )
            wait_job_state(cluster, aid, "PD", timeout=60)
            aggressor.append(aid)
            cluster.scontrol("update", f"JobId={aid}", "Priority=1000000")

        try:
            victim_id, seq = _kill_and_watch(cluster, stage, kill)
            _assert_completing_then_cancelled(victim_id, seq)

            final = wait_job(cluster, aggressor[-1], timeout=120)
            assert final == "CD", (
                f"aggressor must take the slot once it is released; got {final!r}"
            )
        finally:
            for jid in aggressor:
                cluster.cli_allow_fail(["scancel", str(jid)])


class TestMultiNodeCancelWaitsForEveryNode:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_AUTH_ROOT, "partitions": [_PARTITION]}

    def test_job_spanning_two_nodes_waits_in_completing(self, multi_node_cluster):
        cluster = multi_node_cluster
        script = cluster.write_file("span-cancel.sh", _STUBBORN_SCRIPT)

        def stage():
            return parse_job_id(cluster.sbatch(["-N", "2", "--exclusive", script]))

        job_id, seq = _kill_and_watch(cluster, stage, cluster.scancel)
        _assert_completing_then_cancelled(job_id, seq)
        assert len(_show_field(cluster, job_id, "NodeList")) > 0
