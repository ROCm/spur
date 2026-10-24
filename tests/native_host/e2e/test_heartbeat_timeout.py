# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for heartbeat_timeout_secs config propagation and disable.

Validates:
1. A custom timeout propagates: a node whose agent is killed stays up within
   the configured window and only goes Down after it expires.
2. heartbeat_timeout_secs=0 disables down-detection entirely: a killed agent
   never triggers a node-down transition.
"""

import os
import time

import pytest

from cluster import parse_job_id, wait_node_state


def _kill_agent(cluster, node_index):
    """Kill spurd on a node without graceful deregister (SIGKILL)."""
    node = cluster.nodes[node_index]
    prefix = cluster._sudo_prefix() if cluster.agent_as_root else ""
    node.exec_allow_fail(
        f"{prefix}pkill -9 -f '{cluster.bin_dir}/spurd' 2>/dev/null || true"
    )


class TestHeartbeatTimeoutPropagation:
    """Verify that heartbeat_timeout_secs in spur.conf propagates and controls
    when nodes are marked Down."""

    def test_custom_timeout_respected(self, unstarted_cluster):
        """A 120s timeout keeps a dead agent's node up for >90s but marks it
        Down before 180s."""
        cluster = unstarted_cluster
        timeout_secs = 120
        # Use a fast health tick so the mark-down decision is evaluated promptly.
        cluster.start(config_overrides={
            "controller": {
                "heartbeat_timeout_secs": timeout_secs,
                "health_tick_secs": 10,
            },
        })

        target = cluster.node_names[0]
        states = cluster.sinfo_nodes()
        assert states.get(target, "").lower() in ("idle", "alloc", "mix"), (
            f"node {target} not healthy before test: {states}"
        )

        _kill_agent(cluster, 0)

        # At 60s the node must still be up (timeout is 120s).
        time.sleep(60)
        states = cluster.sinfo_nodes()
        assert states.get(target, "").lower() != "down", (
            f"node {target} marked Down at 60s with a 120s timeout: {states}"
        )

        # Wait for the timeout to expire plus a couple of health ticks.
        time.sleep(timeout_secs - 60 + 30)
        wait_node_state(cluster, target, ["down"], timeout=60)


class TestHeartbeatTimeoutDisable:
    """Verify that heartbeat_timeout_secs=0 disables down-detection."""

    def test_zero_timeout_disables_down_detection(self, unstarted_cluster):
        """With timeout=0 a killed agent's node never transitions to Down."""
        cluster = unstarted_cluster
        cluster.start(config_overrides={
            "controller": {
                "heartbeat_timeout_secs": 0,
                "health_tick_secs": 10,
            },
        })

        target = cluster.node_names[0]
        states = cluster.sinfo_nodes()
        assert states.get(target, "").lower() in ("idle", "alloc", "mix"), (
            f"node {target} not healthy before test: {states}"
        )

        _kill_agent(cluster, 0)

        # Wait well past any reasonable timeout. With detection disabled the
        # node must stay up indefinitely.
        time.sleep(120)
        states = cluster.sinfo_nodes()
        assert states.get(target, "").lower() != "down", (
            f"node {target} was marked Down despite heartbeat_timeout_secs=0: {states}"
        )

    @pytest.mark.skipif(
        not os.environ.get("SPUR_TEST_JOB_SUBMIT"),
        reason="sbatch needs passwd-resolvable user; set SPUR_TEST_JOB_SUBMIT=1",
    )
    def test_zero_timeout_preserves_running_jobs(self, unstarted_cluster):
        """With timeout=0 a running job survives an agent kill."""
        cluster = unstarted_cluster
        cluster.start(config_overrides={
            "controller": {
                "heartbeat_timeout_secs": 0,
                "health_tick_secs": 10,
            },
        })

        target = cluster.node_names[0]
        script = cluster.write_file(
            "survivor.sh", "#!/bin/bash\nsleep 600\necho SURVIVED\n"
        )
        sb = cluster.sbatch(["-J", "survivor", "-N", "1", "-w", target, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed:\n{sb}"

        from cluster import wait_job_state, job_state
        wait_job_state(cluster, job_id, "R", timeout=60)

        _kill_agent(cluster, 0)
        time.sleep(120)

        states = cluster.sinfo_nodes()
        assert states.get(target, "").lower() != "down", (
            f"node {target} went Down despite timeout=0: {states}"
        )
        sq = cluster.squeue_all()
        state = job_state(sq, job_id)
        assert state == "R", (
            f"job {job_id} should still be Running with timeout=0, got {state!r}"
        )
