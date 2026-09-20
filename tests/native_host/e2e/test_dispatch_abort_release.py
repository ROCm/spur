# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E regression coverage for the dispatch-abort release race.

When a multi-node job's admission is aborted because one node never confirms
its dispatch, the controller cancels the node(s) that already launched. If
that node's local allocation isn't actually released by the time the
controller retries a dispatch there, the agent rejects the retry as still in
use, and repeated rejections can hold a job at JobHoldMaxRequeue forever.

This reproduces the abort deterministically (block the controller's own
route to one node, so every dispatch to it genuinely fails at the RPC layer
without the node going administratively DOWN) instead of relying on a
natural race, then asserts the node that *did* launch is usable again within
a short bound — not the multi-minute window this bug used to require.
"""

import re
import time

from cluster import block_agent_port, parse_job_id, wait_job_state

# Comfortably above the connect-timeout a blocked node's dispatch attempt
# takes to fail, well below the multi-minute stall this test guards against.
PROBE_RUNNING_BOUND = 20


class TestDispatchAbortRelease:
    def test_probe_job_dispatches_promptly_after_a_partial_admission_abort(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        script = cluster.write_file(
            "abort-release.sh", "#!/bin/bash\nsleep 60\n"
        )

        victim_nodes = ",".join(cluster.node_names[:2])
        submitted_jobs = []
        with block_agent_port(cluster, node_index=1):
            try:
                sb = cluster.sbatch(
                    ["-J", "abort-victim", "-N", "2", "-w", victim_nodes, script]
                )
                aborted_job = parse_job_id(sb)
                assert aborted_job is not None, f"sbatch failed: {sb}"
                submitted_jobs.append(aborted_job)

                self._wait_for_admission_abort(cluster, aborted_job, timeout=30)

                probe_sb = cluster.sbatch(
                    ["-J", "abort-probe", "-N", "1",
                     "-w", cluster.node_names[0], script]
                )
                probe_job = parse_job_id(probe_sb)
                assert probe_job is not None, f"sbatch failed: {probe_sb}"
                submitted_jobs.append(probe_job)

                wait_job_state(
                    cluster, probe_job, "R", timeout=PROBE_RUNNING_BOUND
                )
            finally:
                for job_id in submitted_jobs:
                    cluster.scancel(job_id)

    @staticmethod
    def _wait_for_admission_abort(cluster, job_id: int, timeout: int) -> None:
        """Poll the controller log for this job's abort, not a fixed sleep."""
        needle = f"aborting admission instead of partially running job_id={job_id} "
        deadline = time.time() + timeout
        while time.time() < deadline:
            log = re.sub(
                r"\x1b\[[0-9;]*m", "",
                cluster.nodes[0].read_file(f"{cluster.log_dir}/spurctld.log"),
            )
            if needle in log:
                return
            time.sleep(0.5)
        raise TimeoutError(
            f"job {job_id}'s admission was never aborted within {timeout}s "
            "— the blocked node's dispatch never failed as expected"
        )
