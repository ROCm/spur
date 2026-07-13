# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for preemption.

Covers the fix where a job repeatedly preempted (PreemptMode=Requeue) must
never be held with JobHoldMaxRequeue: preemption requeues are tracked
separately from failure requeues (dispatch failure/Timeout/NodeFail) and are
never checked against `max_batch_requeue`.
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


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
        }

    def test_chronic_preemption_never_holds_job(self, cluster):
        node0 = cluster.node_names[0]
        low_id = None
        try:
            low_script = cluster.write_file(
                "chronic-low.sh", "#!/bin/bash\nsleep 600\n"
            )
            sb = cluster.sbatch(
                ["-J", "chronic-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", low_script]
            )
            low_id = parse_job_id(sb)
            assert low_id is not None, f"submit failed:\n{sb}"
            wait_job_state(cluster, low_id, "R", timeout=30)

            # One more preemption cycle than max_batch_requeue: if preemption
            # requeues wrongly counted against the failure-requeue budget, the
            # job would be held with JobHoldMaxRequeue by the last cycle.
            cycles = self.MAX_BATCH_REQUEUE + 3
            for i in range(cycles):
                wait_job_state(cluster, low_id, "R", timeout=30)

                hi_script = cluster.write_file(
                    f"chronic-high-{i}.sh", "#!/bin/bash\nsleep 2\n"
                )
                hb = cluster.sbatch(
                    ["-J", f"chronic-high-{i}", "-N", "1", f"--nodelist={node0}",
                     "--exclusive", hi_script]
                )
                hi_id = parse_job_id(hb)
                assert hi_id is not None, f"submit failed:\n{hb}"
                # A high enough priority guarantees preemption eligibility
                # (candidate.priority must be < pending.priority / 2).
                cluster.scontrol("update", f"JobId={hi_id}", "Priority=1000000")

                wait_job_state(cluster, low_id, "PD", timeout=15)
                show = cluster.scontrol("show", "job", str(low_id))
                assert "Reason=JobHoldMaxRequeue" not in show, (
                    f"low-priority job held after preemption cycle {i}:\n{show}"
                )

                state = wait_job(cluster, hi_id, timeout=30)
                assert state == "CD", f"high job {hi_id} did not complete: {state}"

            wait_job_state(cluster, low_id, "R", timeout=30)
            show = cluster.scontrol("show", "job", str(low_id))
            assert "Reason=JobHoldMaxRequeue" not in show, (
                f"low-priority job held after {cycles} preemption cycles "
                f"(max_batch_requeue={self.MAX_BATCH_REQUEUE}):\n{show}"
            )
        finally:
            if low_id is not None:
                cluster.cli_allow_fail(["scancel", str(low_id)])
