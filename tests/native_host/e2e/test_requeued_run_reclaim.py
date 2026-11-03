# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""A run left on a node that was Down when its job was requeued is stopped once
the node reports back, whether the job waits in the queue or runs there again."""

import pytest

from cluster import (
    cut_off_node,
    job_state,
    parse_job_id,
    wait_job_state,
    wait_node_state,
    wait_until,
)

HEARTBEAT_TIMEOUT_SECS = 40
# Missed beats past the timeout, plus the health tick that acts on them.
NODE_DOWN_BOUND = 180
# A 30s heartbeat (the first may fail on the dead connection) or the 30s health tick.
RECLAIM_BOUND = 90


def pid_alive(node, pid):
    """Live and not a zombie; `kill -0` would also count a zombie."""
    state = node.exec_allow_fail(f"awk '{{print $3}}' /proc/{pid}/stat 2>/dev/null").strip()
    return state not in ("", "Z")


def read_pid(node, path, desc):
    return wait_until(
        lambda: node.exec_allow_fail(f"cat {path} 2>/dev/null").strip(),
        timeout_s=60, desc=desc,
    )


class TestRequeuedRunReclaim:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"heartbeat_timeout_secs": HEARTBEAT_TIMEOUT_SECS}}

    def _run_pinned(self, cluster, script):
        """Runs a requeueable `script` pinned to node 1 and waits for it to start."""
        name = cluster.node_names[1]
        out = cluster.sbatch([
            "-N", "1", "-w", name, "--requeue", "-t", "30",
            "-o", f"{cluster.remote_dir}/requeued-run.out", script,
        ])
        job_id = parse_job_id(out)
        assert job_id is not None, out
        wait_job_state(cluster, job_id, "R")
        return job_id

    def test_held_requeued_job_does_not_keep_its_old_run(self, multi_node_cluster):
        cluster = multi_node_cluster
        node = cluster.nodes[1]
        name = cluster.node_names[1]
        pid_file = f"{cluster.remote_dir}/held-run.pid"
        script = cluster.write_file(
            "held-run.sh", f"#!/bin/bash\necho $$ > {pid_file}\nexec sleep 3600\n"
        )
        job_id = None
        try:
            job_id = self._run_pinned(cluster, script)
            old_pid = read_pid(node, pid_file, "the first run's pid")

            with cut_off_node(cluster, 1):
                wait_node_state(cluster, name, ["down"], timeout=NODE_DOWN_BOUND)
                wait_job_state(cluster, job_id, "PD", timeout=60)
                # Held so the job cannot be placed back on the node and mask the old run.
                cluster.scontrol("hold", str(job_id))
                assert pid_alive(node, old_pid), "the run outlives its node going Down"

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
            new_pid = read_pid(node, pid_file, "the new run's pid")
            assert new_pid != old_pid
            assert pid_alive(node, new_pid), "the released job runs again on the recovered node"
        finally:
            if job_id is not None:
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_requeued_job_running_again_on_the_node_leaves_no_old_run(self, multi_node_cluster):
        cluster = multi_node_cluster
        node = cluster.nodes[1]
        name = cluster.node_names[1]
        batch_pid = f"{cluster.remote_dir}/rerun-batch.pid"
        step_pid = f"{cluster.remote_dir}/rerun-step.pid"
        script = cluster.write_file(
            "rerun.sh",
            "#!/bin/bash\n"
            f"rm -f {step_pid}\n"
            f"srun bash -c 'echo $$ > {step_pid}; exec sleep 3600' &\n"
            f"echo $$ > {batch_pid}\n"
            "wait\n",
        )
        job_id = None
        try:
            job_id = self._run_pinned(cluster, script)
            old_batch = read_pid(node, batch_pid, "the first run's batch pid")
            old_step = read_pid(node, step_pid, "the first run's srun step pid")

            with cut_off_node(cluster, 1):
                wait_node_state(cluster, name, ["down"], timeout=NODE_DOWN_BOUND)
                wait_job_state(cluster, job_id, "PD", timeout=60)
                node.exec_allow_fail(f"rm -f {batch_pid}")
                assert pid_alive(node, old_step), "the step outlives its node going Down"

            # Pinned to this node, the job runs here again; the old run must not run beside it.
            wait_job_state(cluster, job_id, "R", timeout=RECLAIM_BOUND + 60)
            new_batch = read_pid(node, batch_pid, "the new run's batch pid")
            assert new_batch != old_batch
            for pid, what in ((old_batch, "batch script"), (old_step, "srun step")):
                wait_until(
                    lambda pid=pid: not pid_alive(node, pid),
                    timeout_s=RECLAIM_BOUND, desc=f"the old run's {what} to be stopped",
                )
            assert pid_alive(node, new_batch), "the new run keeps running"
        finally:
            if job_id is not None:
                cluster.cli_allow_fail(["scancel", str(job_id)])
