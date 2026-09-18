# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests proving a rolling Spur upgrade does not disrupt a managed
k0s cluster (no WireGuard).

Spur daemon restarts (simulating ``rolling_upgrade.yml``) must not break k0s —
the cluster stays ready, Spur's k0s state tracking is preserved, and new
workloads can be scheduled after the upgrade.

The tests use native k0s (kuberouter CNI, no WireGuard) matching the
production topology where CI clusters run without WireGuard.

See also ``TestAgentRestartPreservesRunningJob`` in ``test_deregistration.py``
for the no-k0s proof (job survival across agent restart).
"""

from __future__ import annotations

import time

import pytest

from cluster import SpurCluster, make_remote_dir, parse_job_id, wait_job


# --- helpers ----------------------------------------------------------------


def _assert_k0s_phase(c: SpurCluster, expected: str) -> None:
    status = c.k8s_status()
    assert expected in status.lower(), (
        f"k0s phase is not {expected}:\n{status}"
    )


def _assert_spur_tracks_k0s(c: SpurCluster) -> None:
    """Assert Spur still knows about the k0s cluster (phase, members)."""
    status = c.k8s_status()
    assert "members:" in status, (
        f"Spur lost k0s membership after restart:\n{status}"
    )
    nodes = c.sinfo_nodes()
    assert len(nodes) == len(c.node_names), (
        f"Spur lost track of nodes: expected {c.node_names}, "
        f"got {list(nodes.keys())}"
    )


def _restart_agent_and_wait(c: SpurCluster, node_index: int) -> None:
    """Restart spurd on one node and wait for re-registration."""
    node_name = c.node_names[node_index]
    c.restart_agent(node_index=node_index)
    deadline = time.time() + 60
    while time.time() < deadline:
        if node_name in c.sinfo_nodes():
            return
        time.sleep(2)
    raise AssertionError(
        f"{node_name} did not re-register within 60s after agent restart"
    )


def _reset_k0s_all_nodes(c: SpurCluster) -> None:
    """Forcibly reset k0s on every node (same pattern as conftest.py)."""
    sudo = c._sudo_prefix()
    for node in c.nodes:
        node.exec_allow_fail(
            f"{sudo}systemctl stop k0scontroller k0sworker 2>/dev/null || true"
        )
        node.exec_allow_fail(f"{sudo}k0s stop 2>/dev/null || true")
        node.exec_allow_fail(f"{sudo}k0s reset 2>/dev/null || true")
        node.exec_allow_fail(
            f"{sudo}rm -rf /etc/k0s /var/lib/k0s /run/k0s 2>/dev/null || true"
        )


# --- fixture ----------------------------------------------------------------


@pytest.fixture
def k0s_native_cluster(ssh_nodes, remote_bin_dir):
    """Native k0s cluster (no WireGuard, kuberouter CNI), rootful, >= 3 nodes.

    Stands up a Spur cluster with k0s enabled but does NOT call ``k8s up`` —
    the test controls timing so it can assert state before and after each
    upgrade step.
    """
    if len(ssh_nodes) < 3:
        pytest.skip(
            f"upgrade k0s tests require >= 3 nodes for etcd quorum "
            f"(got {len(ssh_nodes)})"
        )
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    c.provision()
    c.root_agent_preflight()
    _reset_k0s_all_nodes(c)
    try:
        c.start(
            config_overrides={
                "cluster": {"enabled": True, "cni": "kuberouter"},
                "auth": {"allow_root_jobs": True},
            },
            agent_as_root=True,
        )
    except Exception:
        c.teardown()
        raise
    yield c
    try:
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)
    except Exception:
        pass
    c.teardown()
    _reset_k0s_all_nodes(c)


# --- tests ------------------------------------------------------------------


@pytest.mark.k0s
class TestRollingUpgradePreservesK0s:
    """Prove that a rolling Spur upgrade (daemon restarts) does not disrupt a
    managed k0s cluster running without WireGuard.

    The sequence mirrors ``rolling_upgrade.yml``:
    1. Bring k0s up, wait for the spur-level ``ready`` phase.
    2. Restart spurctld — verify phase and membership preserved.
    3. Restart spurd on each node one at a time — same assertions.
    4. Bring k0s down so nodes are no longer reserved for kubernetes,
       then submit a batch job to verify scheduling still works.

    Note: ``k8s up`` reserves all scoped nodes for kubernetes (they show as
    reserved in the scheduler and cannot run batch jobs). The batch-job step
    therefore runs after ``k8s down``, which is also what a real operator does
    when validating a rolling upgrade on a cluster that time-shares between
    k8s and batch workloads.
    """

    def test_rolling_upgrade_preserves_k0s_and_scheduling(self, k0s_native_cluster):
        c = k0s_native_cluster
        cp_node = c.node_names[0]

        # --- bring k0s up and wait for spur-level ready ---
        out = c.k8s_up(["--control-plane-node", cp_node])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        members_before = c.k8s_members()

        # --- step 1: controller restart (rolling_upgrade play 3) ---
        c.restart_controller()

        _assert_k0s_phase(c, "ready")
        _assert_spur_tracks_k0s(c)
        assert c.k8s_members() == members_before, (
            "k0s membership changed after controller restart"
        )

        # --- step 2: rolling agent restart (rolling_upgrade play 4) ---
        for i in range(len(c.nodes)):
            _restart_agent_and_wait(c, i)

        _assert_k0s_phase(c, "ready")
        _assert_spur_tracks_k0s(c)
        assert c.k8s_members() == members_before, (
            "k0s membership changed after rolling agent restart"
        )

        # --- step 3: bring k8s down, then verify batch scheduling ---
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)
        c.wait_ready(timeout=120)

        out_path = f"{c.remote_dir}/post-upgrade.out"
        script = c.write_file("post-upgrade-job.sh",
                              "#!/bin/bash\necho UPGRADE_OK\n",
                              all_nodes=True)
        out = c.sbatch(["--job-name=post-upgrade", "-o", out_path, script])
        job_id = parse_job_id(out)
        assert job_id is not None, f"sbatch did not return a job id: {out}"

        state = wait_job(c, job_id, timeout=120)
        if state not in ("CD", "GONE"):
            diag = c.cli_allow_fail(["scontrol", "show", "job", str(job_id)])
            assert False, (
                f"post-upgrade job {job_id} state {state}, expected CD\n{diag}"
            )

        output = c.read_output_on_any_node(out_path)
        assert "UPGRADE_OK" in output, (
            f"post-upgrade job output missing marker:\n{output}"
        )
