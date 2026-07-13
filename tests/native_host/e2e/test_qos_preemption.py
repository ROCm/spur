# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for QOS-driven priority and preemption (SPUR-40).

Covers the fix where a QOS's `priority` delta is wired into a job's
effective priority and a QOS's `preempt_mode` can override the partition's,
letting a plain `sacctmgr`-configured QOS (no `scontrol update Priority=`
trickery) drive real preemption over the wire, on top of a partition that
has already opted into preemption at all (`preempt_mode` != Off, the
partition-level gate `try_preempt` checks before any QOS override is
ever consulted).

Requires Postgres on node 0 (the accounting_cluster fixture, which skips
when Docker is unavailable).
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


class TestQosPriorityPreemption:
    """A low-QOS running job must be preempted by a high-QOS pending job
    contending for the same exclusive node, driven purely by the QOS
    priority delta and the low QOS's preempt_mode override, once the
    partition has opted into preemption at all."""

    @pytest.fixture
    def cluster_config_overrides(self):
        # preempt_mode must be non-Off for the scheduler to attempt
        # preemption at all (a partition-level gate checked before any
        # QOS override is consulted). Set to `cancel` here, deliberately
        # different from `low`'s QOS-level `preemptmode=requeue` below, so
        # the final assertion (low comes back as R, not cancelled) actually
        # exercises the QOS override rather than just the partition default.
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

        # `low`'s negative priority delta keeps its effective priority at
        # the floor, and its preempt_mode override changes the eviction
        # action from the partition's `cancel` to `requeue` (see
        # cluster_config_overrides above for the partition-level opt-in).
        c.sacctmgr(["add", "qos", "name=low", "priority=-1000", "preemptmode=requeue"])
        c.sacctmgr(["add", "qos", "name=high", "priority=100000"])
        # Wait past the QoS cache refresh floor (10s) before submitting.
        time.sleep(15)

        low_id = None
        try:
            low_script = c.write_file("qos-preempt-low.sh", "#!/bin/bash\nsleep 600\n")
            low_out = c.sbatch(
                ["-J", "qos-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "low", low_script]
            )
            low_id = parse_job_id(low_out)
            assert low_id is not None, f"submit failed:\n{low_out}"
            wait_job_state(c, low_id, "R", timeout=30)

            high_script = c.write_file("qos-preempt-high.sh", "#!/bin/bash\nsleep 2\n")
            high_out = c.sbatch(
                ["-J", "qos-high", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "high", high_script]
            )
            high_id = parse_job_id(high_out)
            assert high_id is not None, f"submit failed:\n{high_out}"

            # The node is fully allocated to `low`, so `high` can only start
            # once the scheduler's preemption pass evicts it.
            wait_job_state(c, low_id, "PD", timeout=30)
            high_state = wait_job(c, high_id, timeout=30)
            assert high_state == "CD", f"high-QoS job did not complete: {high_state}"

            # preempt_mode=requeue: `low` must come back, not stay cancelled.
            wait_job_state(c, low_id, "R", timeout=30)
        finally:
            if low_id is not None:
                c.cli_allow_fail(["scancel", str(low_id)])
