# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: nodes claimed by the managed k8s cluster are excluded from Spur batch
scheduling, and return to scheduling after teardown (SPUR-114).

Rootless: `spur k8s up` still assigns roles (which the scheduler gate reads)
without agents bringing up real k0s, so no systemd/sudo/etcd is needed — this
asserts scheduler behavior, not k0s liveness.
"""

import re
import time

import pytest

from cluster import parse_job_id, job_state, wait_job

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
    """Rootless cluster with `[cluster].enabled=true` so `spur k8s up` assigns
    roles without a real k0s bring-up. Tears the cluster down on exit so no k0s
    role/state leaks into a later e2e run."""
    unstarted_cluster.start(config_overrides={"cluster": {"enabled": True}})
    yield unstarted_cluster
    try:
        unstarted_cluster.k8s_down(reset=True)
        unstarted_cluster.wait_k8s_phase("down", timeout=60)
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

        # Teardown reclaims the node -> the pending job schedules and completes.
        cluster.k8s_down(reset=True)
        wait_job(cluster, held, timeout=180)
