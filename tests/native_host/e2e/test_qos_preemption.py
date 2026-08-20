# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for QOS-driven priority and preemption.

Every state transition is verified through both squeue (the primary user-facing
queue view) and scontrol show job (the detailed record view) so that a divergence
between the two surfaces is caught as a test failure rather than silently masked.

Requires Postgres on node 0 (the accounting_cluster fixture, which skips
when Docker is unavailable).

NOTE: REST API cross-verification is not yet covered here — the e2e infrastructure
does not have a REST client helper. That is tracked as a separate gap.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = cluster.scontrol("show", "job", str(job_id))
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


class TestQosPriorityPreemption:
    """A low-QOS running job must be preempted by a high-QOS pending job
    contending for the same exclusive node, driven purely by the QOS
    priority delta and the low QOS's preempt_mode override, once the
    partition has opted into preemption at all."""

    @pytest.fixture
    def cluster_config_overrides(self):
        # preempt_mode must be non-Off for the scheduler to attempt preemption.
        # Set to `cancel` here, deliberately different from `low`'s QOS-level
        # `preemptmode=requeue`, so the final assertion (low comes back as R,
        # not cancelled) exercises the QOS override rather than just the partition default.
        return {
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "preempt_mode": "cancel",
                }
            ],
        }

    def test_high_qos_preempts_low_qos_job(self, accounting_cluster):
        c = accounting_cluster
        node0 = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=low", "priority=-1000", "preemptmode=requeue"])
        c.sacctmgr(["add", "qos", "name=high", "priority=100000"])
        # Wait past the QoS cache refresh floor (10s) before submitting.
        time.sleep(15)

        low_id = None
        high_id = None
        try:
            low_script = c.write_file("qos-preempt-low.sh", "#!/bin/bash\nsleep 600\n")
            low_out = c.sbatch(
                ["-J", "qos-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "low", low_script]
            )
            low_id = parse_job_id(low_out)
            assert low_id is not None, f"submit failed:\n{low_out}"
            wait_job_state(c, low_id, "R", timeout=30)
            _assert_scontrol_state(c, low_id, "RUNNING", "low initial")

            high_script = c.write_file("qos-preempt-high.sh", "#!/bin/bash\nsleep 2\n")
            high_out = c.sbatch(
                ["-J", "qos-high", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "high", high_script]
            )
            high_id = parse_job_id(high_out)
            assert high_id is not None, f"submit failed:\n{high_out}"

            # Node fully occupied by `low`; `high` must be pending before preemption fires.
            wait_job_state(c, high_id, "PD", timeout=30)
            _assert_scontrol_state(c, high_id, "PENDING", "high before preemption")

            # `low` must be requeued (PD), not cancelled.
            wait_job_state(c, low_id, "PD", timeout=30)
            _assert_scontrol_state(c, low_id, "PENDING", "low after requeue")

            # `high` must take the freed slot and start running.
            wait_job_state(c, high_id, "R", timeout=30)
            _assert_scontrol_state(c, high_id, "RUNNING", "high after preemption")

            # `low` must stay pending while `high` holds the node.
            assert job_state(c.squeue_all(), low_id) == "PD", (
                "requeued low-QoS job must stay pending while high-QoS job runs"
            )
            _assert_scontrol_state(c, low_id, "PENDING", "low while high runs")

            high_state = wait_job(c, high_id, timeout=30)
            assert high_state == "CD", f"high-QoS job did not complete: {high_state}"

            # preempt_mode=requeue: `low` must restart once the node is free.
            wait_job_state(c, low_id, "R", timeout=30)
            _assert_scontrol_state(c, low_id, "RUNNING", "low after resuming")
        finally:
            if low_id is not None:
                c.cli_allow_fail(["scancel", str(low_id)])
            if high_id is not None:
                c.cli_allow_fail(["scancel", str(high_id)])


class TestQosPreemptModeOverride:
    """A QOS's preempt_mode must override the partition's preempt_mode when the two
    disagree: a victim whose QOS says cancel must be cancelled even when the partition
    would otherwise requeue it."""

    _CACHE_WARMUP_SECS = 15

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "preempt_mode": "requeue",
                }
            ],
        }

    def test_qos_preempt_mode_cancel_overrides_partition_requeue(self, accounting_cluster):
        """Partition says requeue, but the victim's QOS says cancel.
        The victim must be cancelled (not requeued) when preempted."""
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=fragile", "priority=-1000", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=strong",  "priority=100000"])
        time.sleep(self._CACHE_WARMUP_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("qos-cancel-victim.sh", "#!/bin/bash\nsleep 600\n")
            victim_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "fragile", victim_script])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            aggressor_script = c.write_file("qos-cancel-aggressor.sh", "#!/bin/bash\nsleep 2\n")
            aggressor_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "strong", aggressor_script])
            )
            assert aggressor_id is not None, "aggressor submit failed"

            wait_job_state(c, aggressor_id, "PD", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor before preemption")

            # Victim's QOS says cancel — it must be cancelled, not requeued.
            terminal = wait_job(c, victim_id, timeout=30)
            assert terminal in ("CA", "GONE"), (
                f"victim QOS preempt_mode=cancel must result in cancellation; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, victim_id, "CANCELLED", "victim after preemption")

            # Aggressor must take the freed slot and start running.
            wait_job_state(c, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "RUNNING", "aggressor after preemption")

            # While aggressor is running, the cancelled victim must not have reappeared.
            recheck = job_state(c.squeue_all(), victim_id)
            assert recheck not in ("PD", "R"), (
                f"cancelled victim must not reappear as pending; got {recheck!r}"
            )

            final = wait_job(c, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete successfully; got {final!r}"
        finally:
            if victim_id is not None:
                c.cli_allow_fail(["scancel", str(victim_id)])
            if aggressor_id is not None:
                c.cli_allow_fail(["scancel", str(aggressor_id)])


class TestQosPreemptModeOff:
    """A victim job whose QOS has preempt_mode=off must not be evicted even when
    the partition is configured to cancel running jobs and the pending job has a
    much higher priority."""

    _CACHE_WARMUP_SECS = 15
    _GUARD_SECS = 10

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "preempt_mode": "cancel",
                }
            ],
        }

    def test_qos_preempt_mode_off_blocks_partition_cancel(self, accounting_cluster):
        """Even with a cancel-enabled partition and a vastly higher-priority pending job,
        a running job whose QOS sets preempt_mode=off must not be evicted."""
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=shielded", "priority=-500", "preemptmode=off"])
        c.sacctmgr(["add", "qos", "name=hunter",   "priority=100000"])
        time.sleep(self._CACHE_WARMUP_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("qos-off-victim.sh", "#!/bin/bash\nsleep 600\n")
            victim_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "shielded", victim_script])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            aggressor_script = c.write_file("qos-off-aggressor.sh", "#!/bin/bash\nsleep 5\n")
            aggressor_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "hunter", aggressor_script])
            )
            assert aggressor_id is not None, "aggressor submit failed"

            # Confirm contention is real before the guard period.
            wait_job_state(c, aggressor_id, "PD", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor before guard")

            # Give the scheduler plenty of cycles to (incorrectly) preempt.
            time.sleep(self._GUARD_SECS)

            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "victim with QOS preempt_mode=off must not be evicted by the partition cancel policy"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")

            assert job_state(sq, aggressor_id) == "PD", (
                "aggressor must stay pending — victim's QOS shields it from eviction"
            )
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor after guard")
        finally:
            if victim_id is not None:
                c.cli_allow_fail(["scancel", str(victim_id)])
            if aggressor_id is not None:
                c.cli_allow_fail(["scancel", str(aggressor_id)])
