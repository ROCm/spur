# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for QOS-driven priority and preemption.

Every state transition is verified through both squeue (the primary user-facing
queue view) and scontrol show job (the detailed record view) so that a divergence
between the two surfaces is caught as a test failure rather than silently masked.

Requires:
  - preempt_type=qos_priority (scheduler config); preemption is off otherwise,
    and eligibility is then decided by the QOS allow-list plus QOS rank
  - Postgres on node 0 (the accounting_cluster fixture, which skips when Docker
    is unavailable)

NOTE: REST API cross-verification is not yet covered here — the e2e infrastructure
does not have a REST client helper. That is tracked as a separate gap.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state


# Required when the test runner SSHes in as root: spurd refuses to execute jobs
# as uid 0 unless this is explicitly enabled.
_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}

# QOS cache refreshes on the accounting interval; a freshly added QOS needs a
# cycle before the scheduler acts on it.
_CACHE_WARMUP_SECS = 15

_GUARD_SECS = 12


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = cluster.scontrol("show", "job", str(job_id))
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


class TestQosPriorityPreemption:
    """A low-QOS running job must be preempted by a high-QOS pending job
    contending for the same exclusive node, driven by the high QOS's
    allow-list entry for the low QOS plus its higher QOS rank, and evicted
    according to the low QOS's preempt_mode override."""

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
            "scheduler": {
                "preempt_type": "qos_priority",
            },
            **_AUTH_ROOT,
        }

    def test_high_qos_preempts_low_qos_job(self, accounting_cluster):
        c = accounting_cluster
        node0 = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=low", "priority=-1000", "preemptmode=requeue"])
        c.sacctmgr(["add", "qos", "name=high", "priority=100000", "preempt=low"])
        # Wait past the QoS cache refresh floor (10s) before submitting.
        time.sleep(_CACHE_WARMUP_SECS)

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
            "scheduler": {
                "preempt_type": "qos_priority",
            },
            **_AUTH_ROOT,
        }

    def test_qos_preempt_mode_cancel_overrides_partition_requeue(self, accounting_cluster):
        """Partition says requeue, but the victim's QOS says cancel.
        The victim must be cancelled (not requeued) when preempted."""
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=fragile", "priority=-1000", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=strong",  "priority=100000", "preempt=fragile"])
        time.sleep(_CACHE_WARMUP_SECS)

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
    """QOS preempt_mode=off means 'no override — defer to the partition mode'.
    It is NOT a preemption shield. A victim job whose QOS has preempt_mode=off
    must still be preempted according to the partition's policy.

    This matches Slurm's documented behaviour: PreemptMode=OFF on a QOS is
    equivalent to leaving it unset; the cluster-wide / partition preempt_mode
    takes effect. To prevent a QOS's jobs from being preempted, the QOS must
    simply not appear in any preemptor QOS's allow-list."""

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
            "scheduler": {
                "preempt_type": "qos_priority",
            },
            **_AUTH_ROOT,
        }

    def test_qos_preempt_mode_off_defers_to_partition_cancel(self, accounting_cluster):
        """A victim whose QOS has preempt_mode=off must be cancelled when the
        partition says cancel — off means 'use partition default', not 'shield'."""
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=defer-off", "priority=-500", "preemptmode=off"])
        c.sacctmgr(["add", "qos", "name=hunter", "priority=100000", "preempt=defer-off"])
        time.sleep(_CACHE_WARMUP_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("qos-off-victim.sh", "#!/bin/bash\nsleep 600\n")
            victim_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "defer-off", victim_script])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            aggressor_script = c.write_file("qos-off-aggressor.sh", "#!/bin/bash\nsleep 5\n")
            aggressor_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "hunter", aggressor_script])
            )
            assert aggressor_id is not None, "aggressor submit failed"
            wait_job_state(c, aggressor_id, "PD", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor before preemption")

            # preempt_mode=off on the victim QOS means "defer to partition".
            # Partition says cancel → victim must be cancelled, not shielded.
            terminal = wait_job(c, victim_id, timeout=30)
            assert terminal in ("CA", "GONE"), (
                f"victim with QOS preempt_mode=off must be cancelled per the partition policy; "
                f"got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, victim_id, "CANCELLED", "victim after preemption")

            wait_job_state(c, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "RUNNING", "aggressor after preemption")

            final = wait_job(c, aggressor_id, timeout=30)
            assert final == "CD", f"aggressor must complete; got {final!r}"
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])


class TestPreemptTypeUnsetDisablesPreemption:
    """Preemption is off unless `scheduler.preempt_type = qos_priority`.

    This is the global gate, and it is the one thing left unset here: the
    partition allows cancellation, the hunter QOS allow-lists the victim QOS
    and outranks it 100x, and the pending job is boosted far past the victim's
    raw priority. That is exactly the configuration that evicts in
    TestQosPreemptModeOverride, so nothing may happen here for any reason other
    than the absent preempt_type.
    """

    # Deliberately not part of the eligibility rule any more; boosting it here
    # fails the test loudly if raw job priority ever regains the ability to
    # drive preemption on its own.
    _AGGRESSOR_PRIORITY = 1_000_000

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
            # No "scheduler" section at all: preempt_type falls back to its
            # default, which disables preemption entirely.
            **_AUTH_ROOT,
        }

    def test_absent_preempt_type_blocks_an_otherwise_valid_preemption(
        self, accounting_cluster
    ):
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=gate-victim", "priority=100",
                    "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=gate-hunter", "priority=10000",
                    "preempt=gate-victim"])
        time.sleep(_CACHE_WARMUP_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("gate-victim.sh", "#!/bin/bash\nsleep 600\n")
            victim_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}",
                          "-q", "gate-victim", victim_script])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            aggressor_script = c.write_file("gate-hunter.sh", "#!/bin/bash\nsleep 600\n")
            aggressor_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}",
                          "-q", "gate-hunter", aggressor_script])
            )
            assert aggressor_id is not None, "aggressor submit failed"
            wait_job_state(c, aggressor_id, "PD", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor before guard")

            preempted_before = c.sdiag_jobs_preempted()
            c.scontrol("update", f"JobId={aggressor_id}",
                       f"Priority={self._AGGRESSOR_PRIORITY}")

            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "preemption must stay off while scheduler.preempt_type is unset, "
                "even with a valid allow-list and a 100x QOS rank gap"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the allow-listed, higher-ranked aggressor must wait while the "
                "global preemption gate is off"
            )
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor after guard")
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision may be recorded with preempt_type unset"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
