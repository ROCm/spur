# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for partition preempt_mode behaviour and the QOS rank gate.

Each class covers one user-observable property:

  CancelMode    — a preempted job is permanently removed from the queue
  RequeueMode   — a preempted job returns to PENDING and reruns automatically
  SuspendMode   — a preempted job is frozen in place and keeps its node allocation
  PreemptOff    — no eviction occurs even when every other gate would permit it
  QosRankGate   — an allow-listed victim is evicted only when the pending job's QOS
                  priority is *strictly* higher; equal QOS priority must not displace,
                  and a raw job-priority boost cannot substitute for QOS rank

Preemption is off unless `scheduler.preempt_type = qos_priority`, and eligibility is
decided solely by the QOS allow-list plus QOS rank. Every fixture here therefore turns
the gate on and every test creates a QOS pair, so the partition's preempt_mode (or the
QOS rank, for QosRankGate) is the only variable between classes. A negative test that
merely left the gate off would pass without exercising anything.

Effective *job* priority (fair-share, age, partition priority_tier) no longer affects
preemption eligibility, so the former PriorityTier class — which asserted that a higher
partition priority_tier alone could evict a lower-tier job — has been deleted rather
than rewritten. priority_tier still matters for the active-reservation guard, but
covering that needs reservation fixture plumbing this module does not have.

Every state transition is verified through both squeue (the primary user-facing queue view)
and scontrol show job (the detailed record view) so that a divergence between the two
surfaces is caught as a test failure rather than silently masked.

Requires Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent).

NOTE: REST API cross-verification is not yet covered here — the e2e infrastructure
does not have a REST client helper. That is tracked as a separate gap.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 30  # seconds to wait for preemption to fire
_WAIT_RESUME = 60  # seconds to wait for a suspended/requeued job to come back up
_GUARD_SECS = 10  # seconds to hold before asserting "nothing happened"

# QOS cache refreshes on the accounting interval; a freshly added QOS needs a
# cycle before the scheduler acts on it.
_CACHE_WARMUP_SECS = 15

_PARTITION = {
    "name": "default",
    "state": "UP",
    "default": True,
    "nodes": "ALL",
    "max_time": "24:00:00",
    "default_time": "10:00",
}

# Required when the test runner SSHes in as root: spurd refuses to execute jobs
# as uid 0 unless this is explicitly enabled.
_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}

_VICTIM_QOS = "mode-victim"
_HUNTER_QOS = "mode-hunter"

# Applied to the pending job in the negative tests to prove raw job priority is
# not a substitute for QOS rank: preemption must still be decided by the QOS
# gates alone.
_AGGRESSOR_PRIORITY = 1_000_000


def _config(preempt_mode: str) -> dict:
    """Cluster config with the QOS-priority gate on and one partition mode set."""
    return {
        **_AUTH_ROOT,
        "partitions": [{**_PARTITION, "preempt_mode": preempt_mode}],
        "scheduler": {"preempt_type": "qos_priority"},
    }


def _create_qos_pair(cluster, *, hunter_priority: int = 10000,
                     victim_priority: int = 100) -> None:
    """Victim QOS plus a hunter QOS that allow-lists it.

    Neither QOS sets preemptmode, so the resolved PreemptMode comes from the
    partition — the variable this module varies. The victim is created first
    because CreateQos validates every allow-list name against the QOS table.
    """
    cluster.sacctmgr(["add", "qos", f"name={_VICTIM_QOS}",
                      f"priority={victim_priority}"])
    cluster.sacctmgr(["add", "qos", f"name={_HUNTER_QOS}",
                      f"priority={hunter_priority}", f"preempt={_VICTIM_QOS}"])
    time.sleep(_CACHE_WARMUP_SECS)


def _scontrol_state(cluster, job_id: int) -> str:
    """Return scontrol show job output for cross-verification."""
    return cluster.scontrol("show", "job", str(job_id))


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = _scontrol_state(cluster, job_id)
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


def _run_victim(cluster, node: str, prefix: str) -> int:
    """Submit a long-running victim under the victim QOS and wait for it to run."""
    script = cluster.write_file(f"{prefix}-victim.sh", _SLEEP_SCRIPT)
    victim_id = parse_job_id(
        cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}",
                        "-q", _VICTIM_QOS, script])
    )
    assert victim_id is not None, "victim submit failed"
    wait_job_state(cluster, victim_id, "R", timeout=30)
    _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")
    return victim_id


def _queue_aggressor(cluster, node: str, prefix: str,
                     body: str = _QUICK_SCRIPT) -> int:
    """Queue an aggressor under the hunter QOS behind the victim on the same node."""
    script = cluster.write_file(f"{prefix}-aggressor.sh", body)
    aggressor_id = parse_job_id(
        cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}",
                        "-q", _HUNTER_QOS, script])
    )
    assert aggressor_id is not None, "aggressor submit failed"
    return aggressor_id


class TestCancelMode:
    """preempt_mode=cancel: the evicted job is terminated and must never re-enter the queue."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("cancel")

    def test_preempt_mode_cancel_removes_job_permanently(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_qos_pair(cluster)

        victim_id = _run_victim(cluster, node, "cancel")
        aggressor_id = _queue_aggressor(cluster, node, "cancel")

        try:
            # Victim must be cancelled.
            terminal = wait_job(cluster, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"victim should have been cancelled by the higher-QOS aggressor; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(cluster, victim_id, "CANCELLED", "victim after preemption")

            # Aggressor must take the freed slot and start running.
            wait_job_state(cluster, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(cluster, aggressor_id, "RUNNING", "aggressor after preemption")

            # While aggressor is running, the cancelled victim must not have re-entered the queue.
            recheck = job_state(cluster.squeue_all(), victim_id)
            assert recheck not in ("PD", "R"), (
                f"cancelled job must not re-enter the queue; got {recheck!r}"
            )

            assert cluster.sdiag_jobs_preempted() == 1, (
                "sdiag jobs_preempted counter must be 1 after one cancel-mode preemption"
            )

            final = wait_job(cluster, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestRequeueMode:
    """preempt_mode=requeue: the evicted job returns to PENDING and eventually reruns."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("requeue")

    def test_preempt_mode_requeue_returns_job_to_pending(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_qos_pair(cluster)

        victim_id = _run_victim(cluster, node, "requeue")
        aggressor_id = _queue_aggressor(cluster, node, "requeue")

        try:
            # Victim must be requeued (PD), not cancelled.
            wait_job_state(cluster, victim_id, "PD", timeout=_WAIT_PREEMPT)
            _assert_scontrol_state(cluster, victim_id, "PENDING", "victim after requeue")

            # Aggressor must take the freed slot and start running.
            wait_job_state(cluster, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(cluster, aggressor_id, "RUNNING", "aggressor after preemption")

            # Victim must stay pending while the aggressor holds the node.
            assert job_state(cluster.squeue_all(), victim_id) == "PD", (
                "requeued victim must remain pending while aggressor holds the node"
            )
            _assert_scontrol_state(cluster, victim_id, "PENDING", "victim while aggressor runs")

            assert cluster.sdiag_jobs_preempted() == 1, (
                "sdiag jobs_preempted counter must be 1 after one requeue-mode preemption"
            )

            final = wait_job(cluster, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"

            # Once the node is free, victim must restart automatically.
            wait_job_state(cluster, victim_id, "R", timeout=_WAIT_RESUME)
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after resuming")
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestSuspendMode:
    """preempt_mode=suspend: the preempted job is frozen (SIGSTOP) but its node
    allocation is NOT released — the node stays occupied. The pending aggressor must
    remain pending for as long as the victim holds the node suspended."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("suspend")

    def test_preempt_mode_suspend_freezes_job_and_retains_node(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_qos_pair(cluster)

        victim_id = _run_victim(cluster, node, "suspend")
        aggressor_id = _queue_aggressor(cluster, node, "suspend")

        try:
            # Victim must be suspended (S) — frozen but NOT terminated.
            wait_job_state(cluster, victim_id, "S", timeout=_WAIT_PREEMPT)
            _assert_scontrol_state(cluster, victim_id, "SUSPENDED", "victim after suspend")

            # Suspend retains the node allocation — aggressor must stay pending
            # because the node is still held by the suspended victim.
            time.sleep(_GUARD_SECS)
            sq = cluster.squeue_all()
            assert job_state(sq, victim_id) == "S", (
                "suspended victim must remain frozen, not cancelled or requeued"
            )
            _assert_scontrol_state(cluster, victim_id, "SUSPENDED", "victim still suspended")
            assert job_state(sq, aggressor_id) == "PD", (
                "aggressor must stay pending — suspend does not release the node allocation"
            )
            _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor while victim suspended")
        finally:
            cluster.cli_allow_fail(["scontrol", "resume", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestPreemptOff:
    """preempt_mode=off: no running job may be evicted.

    Everything else that gates preemption is deliberately satisfied here — the
    cluster runs preempt_type=qos_priority, the hunter QOS allow-lists the victim
    QOS and outranks it 100x, and the pending job is boosted far past the victim's
    raw priority. preempt_mode=off is the single remaining reason nothing happens,
    so this test cannot pass by accident of an unconfigured gate.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("off")

    def test_preempt_mode_off_blocks_preemption(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        _create_qos_pair(cluster)

        victim_id = _run_victim(cluster, node, "off")
        aggressor_id = _queue_aggressor(cluster, node, "off", _SLEEP_SCRIPT)

        # Confirm contention is real before we test that nothing changes.
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before guard")

        preempted_before = cluster.sdiag_jobs_preempted()
        cluster.scontrol("update", f"JobId={aggressor_id}", f"Priority={_AGGRESSOR_PRIORITY}")

        try:
            # Let the scheduler run many cycles; nothing should change.
            time.sleep(_GUARD_SECS)

            sq = cluster.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "victim must not be evicted when preemption is disabled on the partition, "
                "even though the QOS allow-list and QOS rank both permit it"
            )
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after guard")

            assert job_state(sq, aggressor_id) == "PD", (
                "aggressor must stay pending — preempt_mode=off blocks eviction"
            )
            _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor after guard")
            assert cluster.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision may be recorded when preempt_mode=off"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestQosRankGate:
    """An allow-listed victim is evicted only when the pending job's QOS priority is
    strictly higher. Equal QOS priority must not displace a running job, and a raw
    job-priority boost cannot make up the difference.

    Both tests here hold the allow-list, the partition mode, and the node contention
    constant and vary only the hunter QOS priority, so the outcome can be attributed
    to the rank comparison and nothing else.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config("cancel")

    def test_equal_qos_priority_does_not_preempt(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        # Same rank on both sides; the allow-list still names the victim, so the
        # strict-greater-than comparison is the only thing left to block eviction.
        _create_qos_pair(cluster, hunter_priority=100, victim_priority=100)

        victim_id = _run_victim(cluster, node, "rank-equal")
        aggressor_id = _queue_aggressor(cluster, node, "rank-equal", _SLEEP_SCRIPT)

        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before guard")

        preempted_before = cluster.sdiag_jobs_preempted()
        # Job priority is not part of the eligibility rule any more; boosting it
        # here fails the test loudly if that ever regresses.
        cluster.scontrol("update", f"JobId={aggressor_id}", f"Priority={_AGGRESSOR_PRIORITY}")

        try:
            time.sleep(_GUARD_SECS)

            sq = cluster.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "an allow-listed victim at equal QOS priority must not be displaced, "
                "not even by a pending job with a far higher raw job priority"
            )
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after guard")

            assert job_state(sq, aggressor_id) == "PD", (
                "the equal-rank aggressor must wait its turn"
            )
            _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor after guard")
            assert cluster.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision may be recorded between QOS of equal priority"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])

    def test_strictly_higher_qos_priority_preempts(self, accounting_cluster):
        cluster = accounting_cluster
        node = cluster.node_names[0]
        # Identical to the test above apart from the hunter's QOS rank. No raw
        # priority boost, so QOS rank is demonstrably what carries the decision.
        _create_qos_pair(cluster, hunter_priority=10000, victim_priority=100)

        victim_id = _run_victim(cluster, node, "rank-higher")
        aggressor_id = _queue_aggressor(cluster, node, "rank-higher")

        try:
            terminal = wait_job(cluster, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"victim should be preempted by the higher-ranked QOS; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(cluster, victim_id, "CANCELLED", "victim after preemption")

            wait_job_state(cluster, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(cluster, aggressor_id, "RUNNING", "aggressor after preemption")

            # Cancelled victim must not reappear while aggressor holds the node.
            recheck = job_state(cluster.squeue_all(), victim_id)
            assert recheck not in ("PD", "R"), (
                f"cancelled victim must not re-enter the queue; got {recheck!r}"
            )

            final = wait_job(cluster, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])
