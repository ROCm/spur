# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for preemption.

Covers the fix where a job repeatedly preempted (PreemptMode=Requeue) must
never be held with JobHoldMaxRequeue: preemption requeues are tracked
separately from failure requeues (dispatch failure/Timeout/NodeFail) and are
never checked against `max_batch_requeue`.

Requires:
  - preempt_type=qos_priority (scheduler config); preemption is off otherwise
  - a QOS pair where the aggressor allow-lists the victim and outranks it
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

# Time allowed for a requeued low-priority job to get back to RUNNING after a
# preemption cycle. Generous because it spans the eligibility hold plus node
# release, scheduler pickup, and spurd relaunch, which is sensitive to CI host
# load — not a latency assertion.
RESCHEDULE_TIMEOUT = 60

# QOS cache refreshes on the accounting interval; a freshly added QOS needs a
# cycle before the scheduler acts on it.
_CACHE_WARMUP_SECS = 15


class TestChronicPreemption:
    """A job preempted more times than max_batch_requeue must stay
    schedulable, never landing in JobHoldMaxRequeue."""

    MAX_BATCH_REQUEUE = 2

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "controller": {"max_batch_requeue": self.MAX_BATCH_REQUEUE},
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
            # Required when the test runner SSHes in as root: spurd refuses to
            # execute jobs as uid 0 unless this is explicitly enabled.
            "auth": {"plugin": "none", "allow_root_jobs": True},
        }

    def test_chronic_preemption_never_holds_job(self, accounting_cluster):
        cluster = accounting_cluster
        node0 = cluster.node_names[0]

        # Neither QOS sets preemptmode, so both defer to the partition's
        # `requeue` — the mode whose requeue accounting is under test.
        cluster.sacctmgr(["add", "qos", "name=chronic-low", "priority=100"])
        cluster.sacctmgr(["add", "qos", "name=chronic-high", "priority=10000",
                          "preempt=chronic-low"])
        time.sleep(_CACHE_WARMUP_SECS)

        low_id = None
        try:
            low_script = cluster.write_file(
                "chronic-low.sh", "#!/bin/bash\nsleep 600\n"
            )
            sb = cluster.sbatch(
                ["-J", "chronic-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "chronic-low", low_script]
            )
            low_id = parse_job_id(sb)
            assert low_id is not None, f"submit failed:\n{sb}"
            wait_job_state(cluster, low_id, "R", timeout=RESCHEDULE_TIMEOUT)

            # One more preemption cycle than max_batch_requeue: if preemption
            # requeues wrongly counted against the failure-requeue budget, the
            # job would be held with JobHoldMaxRequeue by the last cycle.
            cycles = self.MAX_BATCH_REQUEUE + 3
            for i in range(cycles):
                wait_job_state(cluster, low_id, "R", timeout=RESCHEDULE_TIMEOUT)

                hi_script = cluster.write_file(
                    f"chronic-high-{i}.sh", "#!/bin/bash\nsleep 2\n"
                )
                hb = cluster.sbatch(
                    ["-J", f"chronic-high-{i}", "-N", "1", f"--nodelist={node0}",
                     "--exclusive", "-q", "chronic-high", hi_script]
                )
                hi_id = parse_job_id(hb)
                assert hi_id is not None, f"submit failed:\n{hb}"

                wait_job_state(cluster, low_id, "PD", timeout=15)
                show = cluster.scontrol("show", "job", str(low_id))
                assert "Reason=JobHoldMaxRequeue" not in show, (
                    f"low-priority job held after preemption cycle {i}:\n{show}"
                )
                assert f"PreemptedBy={hi_id}" in show, (
                    f"PreemptedBy not set on preempted job at cycle {i}:\n{show}"
                )
                assert "PreemptMode=Requeue" in show, (
                    f"PreemptMode not set on preempted job at cycle {i}:\n{show}"
                )

                state = wait_job(cluster, hi_id, timeout=30)
                assert state == "CD", f"high job {hi_id} did not complete: {state}"

            wait_job_state(cluster, low_id, "R", timeout=RESCHEDULE_TIMEOUT)
            show = cluster.scontrol("show", "job", str(low_id))
            assert "Reason=JobHoldMaxRequeue" not in show, (
                f"low-priority job held after {cycles} preemption cycles "
                f"(max_batch_requeue={self.MAX_BATCH_REQUEUE}):\n{show}"
            )

            preempted = cluster.sdiag_jobs_preempted()
            assert preempted == cycles, (
                f"sdiag jobs_preempted expected {cycles} after {cycles} preemption "
                f"cycles, got {preempted}"
            )
        finally:
            if low_id is not None:
                cluster.cli_allow_fail(["scancel", str(low_id)])
