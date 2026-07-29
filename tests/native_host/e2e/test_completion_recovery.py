# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for durable completion-report redelivery across a controller outage.

If the controller is unreachable when a job finishes (a routine restart or
failover), the agent's completion report must not be permanently lost. The
agent keeps redelivering it, so once the controller returns the job leaves
COMPLETING in the normal sub-second path instead of waiting out the multi-minute
``complete_wait_secs`` force-finish timer.

``complete_wait_secs`` is set high so the force-finish timer cannot be what
finalizes the job — a prompt terminal state after the controller returns proves
the redelivered report did it.
"""

import time

import pytest

from cluster import parse_job_id, wait_job


class TestCompletionRecovery:
    # Well above the time the job spends stopped-then-restarted, so a prompt
    # finish can only come from the redelivered report, not the force-finish.
    COMPLETE_WAIT = 300

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"scheduler": {"complete_wait_secs": self.COMPLETE_WAIT}}

    def test_completion_survives_controller_restart(self, cluster):
        out_path = f"{cluster.remote_dir}/recover.out"
        script = cluster.write_file(
            "recover-job.sh", "#!/bin/bash\nsleep 3\necho RECOVER_OK\n"
        )
        sb = cluster.sbatch(["-J", "recover", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"submit failed:\n{sb}"

        # Stop the controller while the job is still running so the completion is
        # generated during the outage and must be redelivered after restart.
        cluster.stop_controller()
        time.sleep(8)
        cluster.restart_controller()

        # CD within 60s (not the 300s force-finish) proves redelivery finalized it.
        state = wait_job(cluster, job_id, timeout=60)
        assert state == "CD", (
            f"job did not complete promptly after controller restart (got {state}); "
            "the completion report was not redelivered"
        )
        content = cluster.read_output_on_any_node(out_path)
        assert "RECOVER_OK" in content, f"job output missing marker:\n{content}"
