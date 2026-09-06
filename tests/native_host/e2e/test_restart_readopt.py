# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for agent re-adoption of running jobs across a spurd crash (spur#803).

A batch job's workload lives in its own cgroup, so it survives a spurd crash.
Before this fix the restarted agent started with an empty job map: it no longer
tracked the survivor, could not reap it, and the controller still reported it
RUNNING while the processes ran on untracked. The agent now persists a per-job
manifest at launch and re-adopts every survivor (a live cgroup) on startup.

This exercises a *crash* (SIGKILL), not a graceful SIGTERM: SIGTERM makes the
agent deregister and its jobs fail over, which is a different path entirely.
"""

import time

from cluster import job_state, parse_job_id


def _sudo(cluster) -> str:
    return cluster._sudo_prefix() if cluster.agent_as_root else ""


def _cgroup_procs(cluster, job_id: int) -> list[str]:
    """PIDs in the job's cgroup on node 0 (empty once the job is gone)."""
    path = f"/sys/fs/cgroup/spur/job_{job_id}/cgroup.procs"
    out = cluster.nodes[0].exec_allow_fail(
        f"{_sudo(cluster)}cat {path} 2>/dev/null || true"
    )
    return [line for line in out.split() if line.strip()]


def _manifest_exists(cluster, job_id: int) -> bool:
    out = cluster.nodes[0].exec_allow_fail(
        f"{_sudo(cluster)}test -f /var/spool/spur/manifests/{job_id}.json "
        f"&& echo YES || echo NO"
    )
    return "YES" in out


def _crash_and_restart_agent(cluster, node_index: int = 0) -> None:
    """SIGKILL spurd (no deregister) and start it again, as a crash-restart."""
    node = cluster.nodes[node_index]
    node.exec_allow_fail(
        f"{_sudo(cluster)}pkill -9 -f '{cluster.bin_dir}/spurd' 2>/dev/null || true"
    )
    time.sleep(1)
    node.exec(cluster._spurd_start_cmd(node_index))
    time.sleep(5)


def _wait_running(cluster, job_id: int, timeout: int = 60) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if job_state(cluster.squeue_all(), job_id) == "R":
            return
        time.sleep(2)
    raise AssertionError(f"job {job_id} never reached RUNNING within {timeout}s")


class TestRestartReadopt:
    def test_batch_job_survives_agent_crash_and_stays_reapable(self, cgroup_cluster):
        cluster = cgroup_cluster
        node0 = cluster.node_names[0]

        # Ignore SIGTERM so cancelling the job has to reach into its cgroup —
        # which the restarted agent can only do if it re-adopted the job.
        script = cluster.write_file(
            "readopt-long.sh",
            "#!/bin/bash\ntrap '' TERM\nsleep 600\n",
        )
        sb = cluster.sbatch(
            ["-J", "readopt", "-N", "1", "-w", node0,
             "--cpus-per-task=1", "--mem=256", script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        _wait_running(cluster, job_id)
        assert _cgroup_procs(cluster, job_id), (
            "job cgroup should hold the sleep before the crash"
        )
        assert _manifest_exists(cluster, job_id), (
            "agent must persist a re-adoption manifest at launch"
        )

        # Crash the agent (SIGKILL: no deregister, controller still thinks
        # RUNNING) and bring it back. The sleep survives in its cgroup.
        _crash_and_restart_agent(cluster, 0)

        assert _cgroup_procs(cluster, job_id), (
            "job should survive the agent crash (its cgroup outlives spurd)"
        )

        # The re-adopted job is still reapable: scancel reaches its cgroup and
        # kills the SIGTERM-ignoring sleep. Without re-adoption the fresh agent
        # would not know the job and the sleep would leak on a released node.
        cluster.scancel(str(job_id))
        deadline = time.time() + 20
        while time.time() < deadline:
            if not _cgroup_procs(cluster, job_id):
                break
            time.sleep(1)
        else:
            raise AssertionError(
                "scancel after the crash did not reap the re-adopted job's "
                "procs — the agent did not re-adopt it"
            )

        # The manifest is dropped once the job ends, so a later restart does not
        # try to recover a dead job.
        deadline = time.time() + 15
        while time.time() < deadline:
            if not _manifest_exists(cluster, job_id):
                break
            time.sleep(1)
        else:
            raise AssertionError("manifest should be removed after the job ends")
