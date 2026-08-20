# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for partition preempt_mode behaviour and priority thresholds.

Each class covers one user-observable property:

  CancelMode         — a preempted job is permanently removed from the queue
  RequeueMode        — a preempted job returns to PENDING and reruns automatically
  SuspendMode        — a preempted job is frozen in place and thaws once the slot is free
  PreemptOff         — no eviction occurs regardless of how high the pending job's priority is
  PriorityThreshold  — preemption requires a substantially higher priority; equal priority
                       must not displace a running job
  PriorityTier       — a higher partition priority_tier raises effective job priority enough
                       to preempt a running job on a lower-tier partition, even when both
                       jobs carry the same raw sbatch priority

Every state transition is verified through both squeue (the primary user-facing queue view)
and scontrol show job (the detailed record view) so that a divergence between the two
surfaces is caught as a test failure rather than silently masked.

NOTE: REST API cross-verification is not yet covered here — the e2e infrastructure
does not have a REST client helper. That is tracked as a separate gap.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 30   # seconds to wait for preemption to fire
_WAIT_RESUME  = 60   # seconds to wait for a suspended/requeued job to come back up
_GUARD_SECS   = 10   # seconds to hold before asserting "nothing happened"

_PARTITION = {
    "name": "default",
    "state": "UP",
    "default": True,
    "nodes": "ALL",
    "max_time": "24:00:00",
    "default_time": "10:00",
}


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


class TestCancelMode:
    """preempt_mode=cancel: the evicted job is terminated and must never re-enter the queue."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"partitions": [{**_PARTITION, "preempt_mode": "cancel"}]}

    def test_preempt_mode_cancel_removes_job_permanently(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", aggressor])
        )
        # Aggressor must be pending — the node is fully occupied by the victim.
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before preemption")

        cluster.scontrol("update", f"JobId={aggressor_id}", "Priority=1000000")

        try:
            # Victim must be cancelled.
            terminal = wait_job(cluster, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"victim should have been cancelled by the higher-priority aggressor; got {terminal!r}"
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

            final = wait_job(cluster, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestRequeueMode:
    """preempt_mode=requeue: the evicted job returns to PENDING and eventually reruns."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"partitions": [{**_PARTITION, "preempt_mode": "requeue"}]}

    def test_preempt_mode_requeue_returns_job_to_pending(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", aggressor])
        )
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before preemption")

        cluster.scontrol("update", f"JobId={aggressor_id}", "Priority=1000000")

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

            final = wait_job(cluster, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"

            # Once the node is free, victim must restart automatically.
            wait_job_state(cluster, victim_id, "R", timeout=_WAIT_RESUME)
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after resuming")
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestSuspendMode:
    """preempt_mode=suspend: the evicted job is frozen (not terminated) and resumes
    automatically once the aggressor releases the node."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"partitions": [{**_PARTITION, "preempt_mode": "suspend"}]}

    def test_preempt_mode_suspend_freezes_then_resumes_job(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", aggressor])
        )
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before preemption")

        cluster.scontrol("update", f"JobId={aggressor_id}", "Priority=1000000")

        try:
            # Victim must be suspended (S) — frozen, not terminated.
            wait_job_state(cluster, victim_id, "S", timeout=_WAIT_PREEMPT)
            _assert_scontrol_state(cluster, victim_id, "SUSPENDED", "victim after suspend")

            # Aggressor must take the slot and start running while victim is frozen.
            wait_job_state(cluster, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(cluster, aggressor_id, "RUNNING", "aggressor while victim suspended")

            # Victim must still be suspended while aggressor holds the node.
            assert job_state(cluster.squeue_all(), victim_id) == "S", (
                "victim must remain suspended (S) while the aggressor is running"
            )
            _assert_scontrol_state(cluster, victim_id, "SUSPENDED", "victim still suspended")

            final = wait_job(cluster, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"

            # Scheduler must unfreeze victim automatically once the node is free.
            wait_job_state(cluster, victim_id, "R", timeout=_WAIT_RESUME)
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after thaw")
        finally:
            cluster.cli_allow_fail(["scontrol", "resume", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestPreemptOff:
    """preempt_mode=off: no running job may be evicted, regardless of how high the
    pending job's priority is set."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"partitions": [{**_PARTITION, "preempt_mode": "off"}]}

    def test_preempt_mode_off_blocks_preemption(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", aggressor])
        )
        # Confirm contention is real before we test that nothing changes.
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before guard")

        cluster.scontrol("update", f"JobId={aggressor_id}", "Priority=1000000")

        try:
            # Let the scheduler run many cycles; nothing should change.
            time.sleep(_GUARD_SECS)

            sq = cluster.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "victim must not be evicted when preemption is disabled on the partition"
            )
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after guard")

            assert job_state(sq, aggressor_id) == "PD", (
                "aggressor must stay pending — it cannot preempt"
            )
            _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor after guard")
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestPriorityThreshold:
    """Preemption is gated on a meaningful priority gap between the running and pending job.
    A pending job at the same priority as a running job must not displace it."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"partitions": [{**_PARTITION, "preempt_mode": "cancel"}]}

    def test_equal_priority_does_not_trigger_preemption(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        competitor = cluster.write_file("competitor.sh", _SLEEP_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        competitor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", competitor])
        )
        # No priority manipulation: both jobs are submitted under identical conditions
        # and must receive the same effective priority from the scheduler.
        wait_job_state(cluster, competitor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, competitor_id, "PENDING", "competitor before guard")

        try:
            time.sleep(_GUARD_SECS)

            sq = cluster.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "running job must not be displaced by a pending job at equal priority"
            )
            _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim after guard")

            assert job_state(sq, competitor_id) == "PD", (
                "equal-priority pending job must wait its turn"
            )
            _assert_scontrol_state(cluster, competitor_id, "PENDING", "competitor after guard")
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(competitor_id)])

    def test_sufficiently_higher_priority_triggers_preemption(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", aggressor])
        )
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before preemption")

        # Only boost the aggressor. The gap between the victim's default priority
        # and 1000000 is large enough to cross the preemption threshold.
        cluster.scontrol("update", f"JobId={aggressor_id}", "Priority=1000000")

        try:
            terminal = wait_job(cluster, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"victim should be preempted by the much-higher-priority aggressor; got {terminal!r}"
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


class TestPriorityTier:
    """A job on a higher priority_tier partition must gain enough effective priority
    to preempt a running job on a lower-tier partition, with no raw priority
    manipulation required — the tier alone must be the deciding factor."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "partitions": [
                {
                    "name": "standard",
                    "state": "UP",
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "priority_tier": 1,
                    "preempt_mode": "cancel",
                },
                {
                    "name": "premium",
                    "state": "UP",
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "priority_tier": 3,
                    "preempt_mode": "cancel",
                },
            ]
        }

    def test_priority_tier_drives_preemption_across_partitions(self, cluster):
        node = cluster.node_names[0]

        victim = cluster.write_file("victim.sh", _SLEEP_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-p", "standard", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=30)
        _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

        # Same node, same raw priority — only the partition tier differs.
        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-p", "premium", aggressor])
        )
        wait_job_state(cluster, aggressor_id, "PD", timeout=30)
        _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before preemption")

        try:
            terminal = wait_job(cluster, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"standard-tier job must be preempted by a premium-tier job of equal raw priority; "
                f"got {terminal!r}"
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
