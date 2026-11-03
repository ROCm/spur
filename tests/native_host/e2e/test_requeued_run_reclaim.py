# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""A run left on a node that was Down when its job was requeued is stopped once
the node reports back, even while the job is still waiting in the queue."""

import pytest

from cluster import (
    cut_off_node,
    job_state,
    parse_job_id,
    wait_job,
    wait_job_state,
    wait_node_state,
    wait_until,
)

HEARTBEAT_TIMEOUT_SECS = 40
# Missed beats past the timeout, plus the health tick that acts on them.
NODE_DOWN_BOUND = 180
# Heartbeats are every 30s; a beat on the dead connection may fail before one reconnects.
RECLAIM_BOUND = 90


def pid_alive(node, pid):
    return node.exec_allow_fail(f"kill -0 {pid} 2>/dev/null && echo alive").strip() == "alive"


class TestRequeuedRunReclaim:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"heartbeat_timeout_secs": HEARTBEAT_TIMEOUT_SECS}}

    def test_held_requeued_job_does_not_keep_its_old_run(self, multi_node_cluster):
        cluster = multi_node_cluster
        node = cluster.nodes[1]
        name = cluster.node_names[1]
        pid_file = f"{cluster.remote_dir}/requeued-run.pid"
        job_id = None
        try:
            out = cluster.sbatch([
                "-N", "1", "-w", name, "--requeue", "-t", "30",
                "-o", f"{cluster.remote_dir}/requeued-run.out",
                "--wrap", f"echo $$ > {pid_file}; exec sleep 3600",
            ])
            job_id = parse_job_id(out)
            assert job_id is not None, out
            wait_job_state(cluster, job_id, "R")
            old_pid = wait_until(
                lambda: node.exec_allow_fail(f"cat {pid_file} 2>/dev/null").strip(),
                timeout_s=60, desc="the first run's pid",
            )

            with cut_off_node(cluster, 1):
                wait_node_state(cluster, name, ["down"], timeout=NODE_DOWN_BOUND)
                wait_job_state(cluster, job_id, "PD", timeout=60)
                # Held so the job cannot be placed back on the node and mask the old run.
                cluster.scontrol("hold", str(job_id))
                assert pid_alive(node, old_pid), "the run outlives its node going DOWN"

            wait_node_state(cluster, name, ["idle", "mix", "alloc"], timeout=RECLAIM_BOUND)
            wait_until(
                lambda: not pid_alive(node, old_pid),
                timeout_s=RECLAIM_BOUND, desc="the requeued job's old run to be stopped",
            )
            assert job_state(cluster.squeue_all(), job_id) == "PD", (
                "the job itself stays queued; only its stale run is released"
            )

            node.exec_allow_fail(f"rm -f {pid_file}")
            cluster.scontrol("release", str(job_id))
            wait_job_state(cluster, job_id, "R", timeout=120)
            new_pid = wait_until(
                lambda: node.exec_allow_fail(f"cat {pid_file} 2>/dev/null").strip(),
                timeout_s=60, desc="the new run's pid",
            )
            assert new_pid != old_pid
            assert pid_alive(node, new_pid), "the released job runs again on the recovered node"
        finally:
            if job_id is not None:
                cluster.cli_allow_fail(["scancel", str(job_id)])
        assert wait_job(cluster, job_id) == "CA"
