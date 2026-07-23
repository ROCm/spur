# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for node deregistration.

Covers: graceful drain+remove, forced eviction, dead-node auto-cleanup,
agent self-deregistration, and the bug fix for failing jobs on downed nodes.
"""

import time

import pytest

from cluster import parse_job_id, job_state, SpurCluster


def _wait_job_terminal(cluster, job_id, timeout=120):
    """Wait for a job to reach any terminal state including NODE_FAIL."""
    terminal = {"CD", "F", "CA", "TO", "NF", "PR", "DL", "OOM"}
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        sq = cluster.squeue_all()
        state = job_state(sq, job_id)
        if state in terminal:
            return state
        if state is not None:
            last = state
        time.sleep(2)
    raise TimeoutError(
        f"Job {job_id} did not reach terminal state within {timeout}s (last: {last})"
    )


def _wait_node_state(cluster, node_name, target_states, timeout=60):
    """Poll sinfo until a node reaches one of the target states."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            out = cluster.sinfo()
            for line in out.splitlines():
                if node_name in line:
                    for state in target_states:
                        if state in line:
                            return state
        except Exception:
            pass
        time.sleep(2)
    raise TimeoutError(
        f"Node {node_name} did not reach {target_states} within {timeout}s"
    )


def _wait_node_gone(cluster, node_name, timeout=60):
    """Poll sinfo until a node disappears."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            out = cluster.sinfo()
            if node_name not in out:
                return
        except Exception:
            pass
        time.sleep(2)
    raise TimeoutError(f"Node {node_name} still visible after {timeout}s")


def _kill_agent(cluster, node_index):
    """Kill the spurd process on a specific node (SIGKILL, no deregister)."""
    node = cluster.nodes[node_index]
    prefix = cluster._sudo_prefix() if cluster.agent_as_root else ""
    node.exec_allow_fail(
        f"{prefix}pkill -9 -f '{cluster.bin_dir}/spurd' 2>/dev/null || true"
    )


def _sigterm_agent(cluster, node_index):
    """Send SIGTERM to spurd on a specific node (graceful deregister)."""
    node = cluster.nodes[node_index]
    prefix = cluster._sudo_prefix() if cluster.agent_as_root else ""
    node.exec_allow_fail(
        f"{prefix}pkill -TERM -f '{cluster.bin_dir}/spurd' 2>/dev/null || true"
    )


class TestDrainAndRemove:
    """Test graceful drain + remove path."""

    def test_drain_and_remove(self, multi_node_cluster):
        cluster = multi_node_cluster
        node0 = cluster.node_names[0]
        node1 = cluster.node_names[1]

        cluster.cli(["spur", "node", "drain", node0, "--reason", "maintenance"])
        _wait_node_state(cluster, node0, ["drain"])

        out = cluster.sinfo()
        assert "drain" in out.lower()

        script = cluster.write_file(
            "drain_test.sh", "#!/bin/bash\nsleep 1\necho done\n"
        )
        sbatch_out = cluster.sbatch(["-N1", script])
        job_id = parse_job_id(sbatch_out)
        assert job_id is not None
        state = _wait_job_terminal(cluster, job_id)
        assert state == "CD", f"expected COMPLETED, got {state}"

        cluster.cli(["spur", "node", "remove", node0])
        _wait_node_gone(cluster, node0)


class TestForcedEviction:
    """Test forced node removal with running jobs."""

    def test_force_remove_kills_jobs(self, multi_node_cluster):
        cluster = multi_node_cluster
        node0 = cluster.node_names[0]

        script = cluster.write_file(
            "long_sleep.sh", "#!/bin/bash\nsleep 600\n"
        )
        sbatch_out = cluster.sbatch(["-N1", f"--nodelist={node0}", script])
        job_id = parse_job_id(sbatch_out)
        assert job_id is not None

        deadline = time.time() + 30
        while time.time() < deadline:
            sq = cluster.squeue_all()
            if job_state(sq, job_id) == "R":
                break
            time.sleep(1)
        assert job_state(cluster.squeue_all(), job_id) == "R"

        cluster.cli([
            "spur", "node", "remove", node0, "--force",
            "--reason", "eviction test",
        ])

        state = _wait_job_terminal(cluster, job_id)
        assert state == "NF", f"expected NODE_FAIL, got {state}"
        _wait_node_gone(cluster, node0)

    def test_remove_blocked_by_running_jobs(self, multi_node_cluster):
        cluster = multi_node_cluster
        node0 = cluster.node_names[0]

        script = cluster.write_file(
            "block_sleep.sh", "#!/bin/bash\nsleep 600\n"
        )
        sbatch_out = cluster.sbatch(["-N1", f"--nodelist={node0}", script])
        job_id = parse_job_id(sbatch_out)
        assert job_id is not None

        deadline = time.time() + 30
        while time.time() < deadline:
            sq = cluster.squeue_all()
            if job_state(sq, job_id) == "R":
                break
            time.sleep(1)

        out = cluster.cli_allow_fail(["spur", "node", "remove", node0])
        assert "running jobs" in out.lower() or "failed_precondition" in out.lower(), (
            f"expected rejection, got: {out}"
        )

        assert node0 in cluster.sinfo()
        cluster.scancel(str(job_id))


class TestAgentSelfDeregistration:
    """Test SIGTERM-based agent self-deregistration."""

    def test_agent_sigterm_deregisters(self, multi_node_cluster):
        cluster = multi_node_cluster
        node1 = cluster.node_names[1]

        _sigterm_agent(cluster, 1)
        _wait_node_gone(cluster, node1, timeout=30)


class TestAgentRestartPreservesRunningJob:
    """Restarting spurd in place (e.g. a binary upgrade via `systemctl
    restart spurd`) must not interrupt a running job, as long as the
    restart completes within heartbeat_timeout_secs.

    This is the acceptance test for the seamless-upgrade fix: a graceful
    SIGTERM must not force-evict the node (unlike TestForcedEviction, which
    covers the deliberate `--force` admin path), and spurd must reconcile
    the still-running job — and its resource allocation — on restart
    instead of losing track of it and stranding or double-booking the
    node's CPUs.

    NOTE: this currently fails on an unfixed spurd, which force-deregisters
    (and thus evicts) the node on every SIGTERM regardless of running jobs.
    """

    HB_TIMEOUT = 15

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"heartbeat_timeout_secs": self.HB_TIMEOUT}}

    def test_restart_agent_mid_job_survives(self, multi_node_cluster):
        cluster = multi_node_cluster
        node0 = cluster.node_names[0]

        script = cluster.write_file(
            "restart_survive.sh", "#!/bin/bash\nsleep 20\necho done\n"
        )
        sbatch_out = cluster.sbatch(
            ["-N", "1", "-c", "2", f"--nodelist={node0}", script]
        )
        job_id = parse_job_id(sbatch_out)
        assert job_id is not None

        deadline = time.time() + 30
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), job_id) == "R":
                break
            time.sleep(1)
        assert job_state(cluster.squeue_all(), job_id) == "R"

        node_before = cluster.scontrol_show_node(node0)
        assert "CPUAlloc=2" in node_before, node_before

        # Simulate an in-place binary upgrade: graceful SIGTERM + relaunch,
        # well within heartbeat_timeout_secs.
        cluster.restart_agent(0)

        state = job_state(cluster.squeue_all(), job_id)
        assert state not in (None, "NF", "F"), (
            f"job was interrupted by agent restart, state={state}"
        )

        state = _wait_job_terminal(cluster, job_id, timeout=60)
        assert state == "CD", f"expected COMPLETED after restart, got {state}"

        # Allocation must not have been stranded (leaked as still-allocated
        # after the job finished) or double-booked (reserved twice across
        # the restart) — once the job's done, CPUAlloc must fall back to 0.
        node_after = cluster.scontrol_show_node(node0)
        assert "CPUAlloc=0" in node_after, node_after


class TestDownNodeFailsJobs:
    """Verify the bug fix: jobs on downed nodes transition to NODE_FAIL."""

    HB_TIMEOUT = 10

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"heartbeat_timeout_secs": self.HB_TIMEOUT}}

    def test_down_node_fails_jobs(self, multi_node_cluster):
        cluster = multi_node_cluster
        node0 = cluster.node_names[0]

        script = cluster.write_file(
            "down_sleep.sh", "#!/bin/bash\nsleep 600\n"
        )
        sbatch_out = cluster.sbatch(["-N1", f"--nodelist={node0}", script])
        job_id = parse_job_id(sbatch_out)
        assert job_id is not None

        deadline = time.time() + 30
        while time.time() < deadline:
            sq = cluster.squeue_all()
            if job_state(sq, job_id) == "R":
                break
            time.sleep(1)
        assert job_state(cluster.squeue_all(), job_id) == "R"

        _kill_agent(cluster, 0)

        state = _wait_job_terminal(cluster, job_id, timeout=self.HB_TIMEOUT + 30)
        assert state == "NF", f"expected NODE_FAIL, got {state}"
