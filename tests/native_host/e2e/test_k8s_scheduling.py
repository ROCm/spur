# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: nodes claimed by the managed k8s cluster are excluded from Spur batch
scheduling (SPUR-114).

Asserts the scheduler gate end-to-end: once `spur k8s up` assigns a node its
k0s role, a job submitted through the real CLI -> gRPC -> scheduler path pends
with the k8s-reserved reason instead of running on that node.
"""

import re
import time

import pytest

from cluster import parse_job_id, job_state, wait_job

pytestmark = pytest.mark.suite_ha

K8S_RESERVED = "Reserved for Kubernetes cluster"


def _reason(cluster, job_id: int) -> str:
    """Full `Reason=` text from `scontrol show job` (runs to end of line)."""
    out = cluster.scontrol("show", "job", str(job_id))
    m = re.search(r"Reason=(.*)", out)
    return m.group(1).strip() if m else ""


def _wait_gate_active(cluster, script: str, timeout: int = 120) -> int:
    """Submit probes until one pends with the reserved reason (role assignment
    lands a reconcile tick after `k8s up`); returns that pending job's id."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        jid = parse_job_id(cluster.sbatch(["-J", "probe", "-o",
                                           f"{cluster.remote_dir}/probe.out", script]))
        assert jid is not None
        time.sleep(3)
        if K8S_RESERVED in _reason(cluster, jid):
            return jid
        cluster.scancel(str(jid))
    raise TimeoutError(f"k8s gate never became active within {timeout}s:\n{cluster.k8s_status()}")


@pytest.fixture
def k8s_enabled_cluster(unstarted_cluster):
    """Cluster with `[cluster].enabled=true` so `spur k8s up` assigns roles.
    Tears the managed cluster down on exit so no k0s state leaks into a later run
    (the harness also wipes the controller state dir)."""
    unstarted_cluster.start(config_overrides={"cluster": {"enabled": True}})
    yield unstarted_cluster
    try:
        unstarted_cluster.k8s_down(reset=True)
    except Exception:
        pass


class TestK8sSchedulingExclusion:
    def test_k8s_reserved_node_excluded_from_scheduling(self, k8s_enabled_cluster):
        cluster = k8s_enabled_cluster

        # Baseline: a job runs before any k8s claim.
        script = cluster.write_file("k8s-sched.sh", "#!/bin/bash\necho ok\n")
        base = parse_job_id(cluster.sbatch(["-J", "pre-k8s", "-o",
                                            f"{cluster.remote_dir}/pre.out", script]))
        assert base is not None
        wait_job(cluster, base, timeout=90)

        # Claim the node(s) for k8s -> the scheduler gate must exclude them, so a
        # newly submitted job pends with the reserved reason instead of running.
        cluster.k8s_up()
        held = _wait_gate_active(cluster, script)
        assert job_state(cluster.squeue_all(), held) == "PD", (
            f"job should stay pending on a k8s-reserved node:\n{cluster.squeue_all()}"
        )
        cluster.scancel(str(held))
