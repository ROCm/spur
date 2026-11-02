# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for the preemption grace (QOS GraceTime) and KillWait.

A victim whose QOS sets GraceTime is not evicted at once. Its srun steps get SIGCONT
then SIGTERM, the batch script gets nothing, and the victim stays RUNNING on its nodes
until the grace deadline. At the deadline the ordinary preemption runs: a graceful
terminate that waits up to kill_wait_secs before SIGKILL, then the partition's
preempt_mode decides whether the victim is cancelled or requeued.

The victim's batch script launches an srun step that traps SIGTERM, appends to a
marker file and keeps sleeping; the batch script itself traps SIGTERM into a second
marker, so a warning that leaks to the batch script is visible. Time limits share the
same kill wait: a job that ignores SIGTERM sits in COMPLETING for the whole of
kill_wait_secs before it ends TIMEOUT.

Requires Postgres on node 0 for the preemption tests (accounting_cluster fixture,
skips when Docker is absent); the time-limit test needs only the plain cluster.
"""

import re
import time
from datetime import datetime

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state, wait_sacct_row, wait_until

_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}

_PARTITION = {
    "name": "default",
    "state": "UP",
    "default": True,
    "nodes": "ALL",
    "max_time": "24:00:00",
    "default_time": "10:00",
}

_VICTIM_QOS = "grace-victim"
_HUNTER_QOS = "grace-hunter"

_GRACE_SECS = 40
# Above the 5 s floor, and short enough that the final terminate of a victim
# that ignores SIGTERM does not dominate the run time.
_KILL_WAIT_SECS = 10
# The time-limit test uses a wait above the old fixed 30 s so a regression to it shows.
_LONG_KILL_WAIT_SECS = 40

# QOS cache refreshes on the accounting interval; a freshly added QOS needs a
# cycle before the scheduler acts on it.
_CACHE_WARMUP_SECS = 15

# Time for the scheduler to notice the aggressor and deliver the warning.
_WARNING_WAIT_SECS = 30
# How far before the deadline the "still in grace" assertions run. The marker is
# observed a few seconds after the signal, so this leaves room on both sides.
_DEADLINE_MARGIN_SECS = 10
# Headroom for polling latency on top of an expected duration.
_SLACK_SECS = 40

_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"


def _config(preempt_mode: str) -> dict:
    return {
        **_AUTH_ROOT,
        "partitions": [{**_PARTITION, "preempt_mode": preempt_mode}],
        "scheduler": {"preempt_type": "qos_priority", "kill_wait_secs": _KILL_WAIT_SECS},
    }


def _create_graced_qos_pair(cluster) -> None:
    """Victim QOS carrying GraceTime, plus a hunter QOS that allow-lists and outranks it."""
    cluster.sacctmgr(["add", "qos", f"name={_VICTIM_QOS}", "priority=100",
                      f"GraceTime={_GRACE_SECS}"])
    cluster.sacctmgr(["add", "qos", f"name={_HUNTER_QOS}", "priority=10000",
                      f"preempt={_VICTIM_QOS}"])
    time.sleep(_CACHE_WARMUP_SECS)


def _read(cluster, path: str) -> str:
    return cluster.read_output_all_nodes(path)


def _marker_lines(cluster, path: str) -> list[str]:
    return _read(cluster, path).split()


def _show_field(cluster, job_id: int, field: str) -> str:
    show = cluster.scontrol("show", "job", str(job_id))
    match = re.search(rf"\b{field}=(\S+)", show)
    return match.group(1) if match else ""


def _parse_scontrol_time(value: str) -> datetime:
    return datetime.strptime(value, "%Y-%m-%dT%H:%M:%S")


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    show = cluster.scontrol("show", "job", str(job_id))
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


class _Victim:
    """A submitted victim plus the marker files its scripts write."""

    def __init__(self, job_id: int, step_marker: str, batch_marker: str):
        self.job_id = job_id
        self.step_marker = step_marker
        self.batch_marker = batch_marker


def _submit_victim(cluster, prefix: str, *, sbatch_args: list[str],
                   nodes: int = 1, step_on_term: str = ":") -> _Victim:
    """Submit a long-running victim under the graced QOS and wait for it to run.

    *step_on_term* is the shell command the srun step runs after recording SIGTERM;
    ``:`` keeps the step alive, ``exit 0`` makes it leave on the warning.
    """
    step_marker = f"{cluster.remote_dir}/{prefix}-step.marker"
    batch_marker = f"{cluster.remote_dir}/{prefix}-batch.marker"
    step_script = cluster.write_file(
        f"{prefix}-step.sh",
        "#!/bin/bash\n"
        f"trap 'echo CONT >> {step_marker}' CONT\n"
        f"trap 'echo TERM >> {step_marker}; {step_on_term}' TERM\n"
        "while true; do sleep 1; done\n",
        all_nodes=True,
    )
    batch_script = cluster.write_file(
        f"{prefix}-victim.sh",
        "#!/bin/bash\n"
        f"trap 'echo TERM >> {batch_marker}' TERM\n"
        f"srun -N{nodes} {step_script} &\n"
        "pid=$!\n"
        'while kill -0 "$pid" 2>/dev/null; do wait "$pid"; done\n',
    )
    job_id = parse_job_id(
        cluster.sbatch([f"-N{nodes}", "--exclusive", "-q", _VICTIM_QOS, *sbatch_args, batch_script])
    )
    assert job_id is not None, "victim submit failed"
    wait_job_state(cluster, job_id, "R", timeout=60)
    _assert_scontrol_state(cluster, job_id, "RUNNING", "victim initial")
    return _Victim(job_id, step_marker, batch_marker)


def _queue_aggressor(cluster, prefix: str, *, sbatch_args: list[str], nodes: int = 1) -> int:
    script = cluster.write_file(f"{prefix}-aggressor.sh", _QUICK_SCRIPT)
    aggressor_id = parse_job_id(
        cluster.sbatch([f"-N{nodes}", "--exclusive", "-q", _HUNTER_QOS, *sbatch_args, script])
    )
    assert aggressor_id is not None, "aggressor submit failed"
    return aggressor_id


def _wait_for_warning(cluster, victim: _Victim) -> float:
    """Block until the step has recorded SIGTERM; return the local time it was seen."""
    wait_until(
        lambda: "TERM" in _marker_lines(cluster, victim.step_marker),
        timeout_s=_WARNING_WAIT_SECS, interval_s=1,
        desc=f"the preemption warning to reach job {victim.job_id}'s step",
    )
    return time.time()


def _assert_grace_visible(cluster, victim: _Victim) -> None:
    """scontrol shows when the warning went out and a deadline one GraceTime later."""
    show = cluster.scontrol("show", "job", str(victim.job_id))
    preempt_time = re.search(r"\bPreemptTime=(\S+)", show)
    assert preempt_time, f"scontrol show job must show PreemptTime during the grace:\n{show}"
    end_time = re.search(r"\bEndTime=(\S+)", show)
    assert end_time, f"scontrol show job must show EndTime during the grace:\n{show}"
    span = (_parse_scontrol_time(end_time.group(1)) - _parse_scontrol_time(preempt_time.group(1))).total_seconds()
    assert abs(span - _GRACE_SECS) <= 2, (
        f"EndTime must be the grace deadline, {_GRACE_SECS}s after PreemptTime:\n{show}"
    )
    lines = _marker_lines(cluster, victim.step_marker)
    # Bash runs pending traps in signal-number order, so the order sent is not observable here.
    assert "CONT" in lines and "TERM" in lines, (
        f"the step must receive both SIGCONT and SIGTERM, saw {lines}"
    )


def _assert_still_in_grace(cluster, victim: _Victim, aggressor_id: int, warned_at: float) -> None:
    """Shortly before the deadline the victim is untouched and the preemptor still waits."""
    remaining = warned_at + _GRACE_SECS - _DEADLINE_MARGIN_SECS - time.time()
    if remaining > 0:
        time.sleep(remaining)
    sq = cluster.squeue_all()
    assert job_state(sq, victim.job_id) == "R", (
        "the victim must keep running until the grace deadline"
    )
    _assert_scontrol_state(cluster, victim.job_id, "RUNNING", "victim late in grace")
    assert job_state(sq, aggressor_id) == "PD", "the preemptor must wait out the grace"
    assert _marker_lines(cluster, victim.step_marker).count("TERM") == 1, (
        "the warning is sent once, not repeated during the grace"
    )
    assert not _read(cluster, victim.batch_marker).strip(), (
        "the batch script must not receive the warning, only the srun step"
    )


def _assert_no_launch_rejection(cluster) -> None:
    for index in range(len(cluster.nodes)):
        assert "rejecting launch" not in cluster.spurd_log(index), (
            f"spurd on node {index} rejected a launch while the victim was being preempted"
        )


def _assert_sacct_preempted(cluster, victim_id: int, aggressor_id: int) -> None:
    row = wait_sacct_row(cluster, victim_id, "JobID,State,PreemptedBy", timeout=90).split()
    assert row[1] == "PREEMPTED", f"sacct must record the victim as PREEMPTED, got {row}"
    assert row[2] == str(aggressor_id), f"sacct PreemptedBy must name the preemptor, got {row}"


def _finish_window() -> int:
    return _GRACE_SECS + _KILL_WAIT_SECS + _SLACK_SECS


def _cleanup(cluster, *job_ids: int) -> None:
    for job_id in job_ids:
        cluster.cli_allow_fail(["scancel", str(job_id)])


class TestGraceCancelMode:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("cancel")

    def test_grace_cancel_mode(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_graced_qos_pair(cluster)
        victim = _submit_victim(cluster, "cancel", sbatch_args=[f"--nodelist={node}"])
        aggressor_id = _queue_aggressor(cluster, "cancel", sbatch_args=[f"--nodelist={node}"])

        try:
            warned_at = _wait_for_warning(cluster, victim)
            _assert_grace_visible(cluster, victim)
            _assert_still_in_grace(cluster, victim, aggressor_id, warned_at)

            terminal = wait_job(cluster, victim.job_id, timeout=_finish_window())
            assert terminal in ("CA", "GONE"), (
                f"victim must be cancelled once the grace and kill wait end; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(cluster, victim.job_id, "CANCELLED", "victim after preemption")
                assert _show_field(cluster, victim.job_id, "PreemptedBy") == str(aggressor_id)

            wait_job_state(cluster, aggressor_id, "R", timeout=_SLACK_SECS)
            assert wait_job(cluster, aggressor_id, timeout=_SLACK_SECS) == "CD"
            _assert_no_launch_rejection(cluster)
            _assert_sacct_preempted(cluster, victim.job_id, aggressor_id)
            assert cluster.sdiag_jobs_preempted() == 1
        finally:
            _cleanup(cluster, victim.job_id, aggressor_id)


class TestGraceRequeueMode:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("requeue")

    def test_grace_requeue_mode(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_graced_qos_pair(cluster)
        victim = _submit_victim(cluster, "requeue", sbatch_args=[f"--nodelist={node}"])
        aggressor_id = _queue_aggressor(cluster, "requeue", sbatch_args=[f"--nodelist={node}"])

        try:
            warned_at = _wait_for_warning(cluster, victim)
            _assert_grace_visible(cluster, victim)
            _assert_still_in_grace(cluster, victim, aggressor_id, warned_at)

            wait_job_state(cluster, victim.job_id, "PD", timeout=_finish_window())
            _assert_scontrol_state(cluster, victim.job_id, "PENDING", "victim after requeue")

            wait_job_state(cluster, aggressor_id, "R", timeout=_SLACK_SECS)
            assert job_state(cluster.squeue_all(), victim.job_id) == "PD", (
                "the requeued victim must wait while the preemptor holds the node"
            )
            assert wait_job(cluster, aggressor_id, timeout=_SLACK_SECS) == "CD"
            _assert_no_launch_rejection(cluster)
        finally:
            _cleanup(cluster, victim.job_id, aggressor_id)


class TestGraceAgentRestart:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("cancel")

    def test_grace_survives_spurd_restart(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_graced_qos_pair(cluster)
        victim = _submit_victim(cluster, "restart", sbatch_args=[f"--nodelist={node}"])
        aggressor_id = _queue_aggressor(cluster, "restart", sbatch_args=[f"--nodelist={node}"])

        try:
            warned_at = _wait_for_warning(cluster, victim)

            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)

            # The restarted agent must neither re-warn nor cut the grace short.
            _assert_still_in_grace(cluster, victim, aggressor_id, warned_at)

            terminal = wait_job(cluster, victim.job_id, timeout=_finish_window())
            assert terminal in ("CA", "GONE"), (
                f"victim must still be cancelled after the agent restart; got {terminal!r}"
            )
            wait_job_state(cluster, aggressor_id, "R", timeout=_SLACK_SECS)
            assert wait_job(cluster, aggressor_id, timeout=_SLACK_SECS) == "CD"
            _assert_sacct_preempted(cluster, victim.job_id, aggressor_id)
        finally:
            _cleanup(cluster, victim.job_id, aggressor_id)


class TestExitDuringGrace:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("cancel")

    def test_exit_during_grace_is_preempted(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_graced_qos_pair(cluster)
        # A clean exit on the warning would otherwise complete the job.
        victim = _submit_victim(cluster, "exit", sbatch_args=[f"--nodelist={node}"],
                                step_on_term="exit 0")
        aggressor_id = _queue_aggressor(cluster, "exit", sbatch_args=[f"--nodelist={node}"])

        try:
            warned_at = _wait_for_warning(cluster, victim)

            terminal = wait_job(cluster, victim.job_id, timeout=_GRACE_SECS - _DEADLINE_MARGIN_SECS)
            assert time.time() - warned_at < _GRACE_SECS, "the victim must end inside its grace"
            assert terminal in ("CA", "GONE"), (
                f"a victim that exits during its grace is preempted, not completed; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(cluster, victim.job_id, "CANCELLED", "victim after exit")

            wait_job_state(cluster, aggressor_id, "R", timeout=_SLACK_SECS)
            assert wait_job(cluster, aggressor_id, timeout=_SLACK_SECS) == "CD"
            _assert_sacct_preempted(cluster, victim.job_id, aggressor_id)
            assert cluster.sdiag_jobs_preempted() == 1
        finally:
            _cleanup(cluster, victim.job_id, aggressor_id)


@pytest.fixture
def two_node_accounting_cluster(ssh_nodes, request):
    """accounting_cluster, skipped before any deploy when fewer than 2 nodes exist."""
    if len(ssh_nodes) < 2:
        pytest.skip(
            f"Multi-node tests require at least 2 nodes in SPUR_TEST_NODES "
            f"(got {len(ssh_nodes)})"
        )
    return request.getfixturevalue("accounting_cluster")


class TestGracePartialExitMultiNode:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("cancel")

    def test_grace_partial_exit_multi_node(self, two_node_accounting_cluster):
        cluster = two_node_accounting_cluster
        _create_graced_qos_pair(cluster)
        # The flag exists on the controller node only, so only the step there leaves on the warning.
        flag = cluster.write_file("partial-exit.flag", "", executable=False)
        victim = _submit_victim(cluster, "partial", sbatch_args=[], nodes=2,
                                step_on_term=f"[ -e {flag} ] && exit 0")
        aggressor_id = _queue_aggressor(cluster, "partial", sbatch_args=[], nodes=2)

        try:
            _wait_for_warning(cluster, victim)

            terminal = wait_job(cluster, victim.job_id, timeout=_finish_window())
            assert terminal in ("CA", "GONE"), (
                f"a victim whose step left on one node must still be preempted; got {terminal!r}"
            )

            wait_job_state(cluster, aggressor_id, "R", timeout=_SLACK_SECS)
            assert "NumNodes=2" in cluster.scontrol("show", "job", str(aggressor_id)), (
                "the preemptor must get both nodes the victim held"
            )
            assert wait_job(cluster, aggressor_id, timeout=_SLACK_SECS) == "CD"
            _assert_no_launch_rejection(cluster)
            _assert_sacct_preempted(cluster, victim.job_id, aggressor_id)
        finally:
            _cleanup(cluster, victim.job_id, aggressor_id)


class TestTimeLimitKillWait:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            **_AUTH_ROOT,
            "scheduler": {"kill_wait_secs": _LONG_KILL_WAIT_SECS},
        }

    def test_time_limit_uses_kill_wait(self, cluster):
        script = cluster.write_file(
            "kill-wait-stubborn.sh",
            "#!/bin/bash\ntrap '' TERM\nsleep 300\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "kill-wait-tl", "--time=00:00:20", script]))
        assert job_id is not None

        try:
            # The watchdog runs on a tick, so allow for it on top of the 20 s limit.
            wait_job_state(cluster, job_id, "CG", timeout=_SLACK_SECS + 20)
            in_completing_since = time.time()

            # The old fixed 30 s wait would already have ended the job by now.
            remaining = in_completing_since + 31 - time.time()
            if remaining > 0:
                time.sleep(remaining)
            assert job_state(cluster.squeue_all(), job_id) == "CG", (
                "a job ignoring SIGTERM must stay COMPLETING for the whole configured kill wait"
            )

            assert wait_job(cluster, job_id, timeout=_LONG_KILL_WAIT_SECS) == "TO", (
                f"the job must end TIMEOUT once the kill wait is over:\n{cluster.debug_job(job_id)}"
            )
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobState=TIMEOUT" in show, show
            assert "Reason=TimeLimit" in show, show
        finally:
            _cleanup(cluster, job_id)
