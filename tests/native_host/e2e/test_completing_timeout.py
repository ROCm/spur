# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for the completing-timeout node-cancel fix.

When a multi-node job stalls in COMPLETING because one node never reports
(a lost completion report, or a task that outlives the run), the controller
force-finishes it after ``complete_wait_secs`` and frees that node's resources
in its own accounting. If it does not also tell the node's agent to stop the
job, the agent keeps the stale local allocation and rejects the next dispatch
("controller-allocated GPUs unavailable"), stranding the node until the job
requeues to JobHoldMaxRequeue.

This reproduces the stall deterministically without GPUs: rank 0 fails, which
ends the run and has Spur stop rank 1. Rank 1 ignores that SIGTERM and sleeps
on a unique marker, and the kill wait outlasts the shortened completing wait,
so the job sits in COMPLETING until the timeout force-finishes it. The fix must
deliver a cancel to rank 1's node, killing the orphaned marker process.

Requires at least 2 nodes in SPUR_TEST_NODES.
"""

import time

import pytest

from cluster import parse_job_id, wait_job


class TestCompletingTimeoutCancel:
    # Well above the 10s force-finish tick, low enough to keep the test fast.
    COMPLETE_WAIT = 15
    # Far past the completing wait, so the force-finish, not the stop, ends rank 1.
    KILL_WAIT = 120

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "scheduler": {
                "complete_wait_secs": self.COMPLETE_WAIT,
                "kill_wait_secs": self.KILL_WAIT,
            }
        }

    def test_completing_timeout_cancels_stuck_node(self, multi_node_cluster):
        cluster = multi_node_cluster

        # Unique duration so pgrep -f matches only this test's process.
        marker = "8675309"
        script = cluster.write_file(
            "completing-stuck.sh",
            "#!/bin/bash\n"
            # Rank 0's failure ends the run -> job enters COMPLETING.
            'if [ "${SPUR_NODE_RANK}" = "0" ]; then exit 3; fi\n'
            # Rank 1 shrugs off the stop and never reports.
            "trap '' TERM\n"
            f"sleep {marker}\n",
        )
        sb = cluster.sbatch(["-J", "completing-stuck", "-N", "2", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        # Force-finish sets a job with an unreported node to Failed; reaching a
        # terminal state confirms the completing-timeout path ran.
        state = wait_job(cluster, job_id, timeout=self.COMPLETE_WAIT + 60)
        assert state in ("F", "GONE"), (
            f"expected force-finish to a terminal state, got {state}"
        )
        # The unreported node is quarantined before its allocation is released.
        drained = [
            name for name in cluster.node_names
            if "job cleanup was not confirmed before completion timeout"
            in cluster.scontrol_show_node(name)
        ]
        assert len(drained) == 1, (
            f"expected exactly the unreported node drained, got {drained}"
        )

        # The orphaned marker on rank 1's node must be gone: the fix's cancel
        # killed the process and freed the local allocation. Poll briefly to let
        # the cancel RPC and the agent's reap land.
        deadline = time.time() + 30
        still_running = True
        while time.time() < deadline:
            if not self._marker_running(cluster, marker):
                still_running = False
                break
            time.sleep(2)

        assert not still_running, (
            f"marker process 'sleep {marker}' survived force-finish — the "
            "completing-timeout cancel never reached the stuck node, so its "
            "agent still holds the stale allocation"
        )

    @staticmethod
    def _marker_running(cluster, marker: str) -> bool:
        # Bracket the first char so the pgrep pattern cannot match its own
        # `bash -c "pgrep -f ..."` command line (a classic self-match footgun).
        for node in cluster.nodes:
            out = node.exec_allow_fail(f"pgrep -f '[s]leep {marker}' || true")
            if out.strip():
                return True
        return False
