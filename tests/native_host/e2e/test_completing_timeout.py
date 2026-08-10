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

This reproduces the stall deterministically without GPUs: rank 0 exits (and
reports), rank 1 sleeps on a unique marker and never reports, so the job sits
in COMPLETING until the shortened timeout force-finishes it. The fix must
deliver a cancel to rank 1's node, killing the orphaned marker process.

Requires at least 2 nodes in SPUR_TEST_NODES.
"""

import time

import pytest

from cluster import parse_job_id, wait_job


class TestCompletingTimeoutCancel:
    # Well above the 10s force-finish tick, low enough to keep the test fast.
    COMPLETE_WAIT = 15

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"scheduler": {"complete_wait_secs": self.COMPLETE_WAIT}}

    def test_completing_timeout_cancels_stuck_node(self, multi_node_cluster):
        cluster = multi_node_cluster

        # Unique duration so pgrep -f matches only this test's process.
        marker = "8675309"
        script = cluster.write_file(
            "completing-stuck.sh",
            "#!/bin/bash\n"
            # Rank 0 exits immediately -> reports -> job enters COMPLETING.
            'if [ "${SPUR_NODE_RANK}" = "0" ]; then exit 0; fi\n'
            # Rank 1 outlives the run and never reports.
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
