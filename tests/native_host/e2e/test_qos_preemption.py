# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for QOS-driven priority and preemption (SPUR-40).

Covers the fix where a QOS's `priority` delta is wired into a job's
effective priority and a QOS's `preempt_mode` can override the partition's,
letting a plain `sacctmgr`-configured QOS (no `scontrol update Priority=`
trickery, no partition `preempt_mode`) drive real preemption over the wire.

Requires Postgres on node 0 (the accounting_cluster fixture, which skips
when Docker is unavailable).
"""

import time

from cluster import parse_job_id, wait_job, wait_job_state


class TestQosPriorityPreemption:
    """A low-QOS running job must be preempted by a high-QOS pending job
    contending for the same exclusive node, driven purely by the QOS
    priority delta and the low QOS's preempt_mode override."""

    def test_high_qos_preempts_low_qos_job(self, accounting_cluster):
        c = accounting_cluster
        node0 = c.node_names[0]

        # `low`'s negative priority delta keeps its effective priority at
        # the floor, and its preempt_mode override makes it preemptable
        # without needing a partition-level preempt_mode at all.
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
