# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: nodes claimed by the managed k8s cluster are excluded from Spur batch
scheduling, and return to scheduling after teardown (SPUR-114).

Uses rootless spurd: `spur k8s up` still assigns k0s roles on the controller
(the scheduler gate reads those), while the agents never bring up real k0s, so
no systemd/sudo/etcd is required. The cluster stays in `provisioning`, which is
all this test needs — it asserts scheduler behavior, not k0s liveness.
"""

import re
import time

import pytest

from cluster import parse_job_id, job_state, wait_job

K8S_RESERVED = "Reserved for Kubernetes cluster"


def _reason(cluster, job_id: int) -> str:
    """Full `Reason=` text from `scontrol show job` (it may contain spaces/commas
    and runs to end of line)."""
    out = cluster.scontrol("show", "job", str(job_id))
    m = re.search(r"Reason=(.*)", out)
    return m.group(1).strip() if m else ""


def _wait_gate_active(cluster, script: str, timeout: int = 90) -> int:
    """The k0s role is assigned a reconcile tick after `k8s up`; before that a job
    still runs. Submit probes until one pends with the reserved reason, proving the
    gate is live, and return that still-pending job's id."""
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
    """Rootless cluster with `[cluster].enabled=true` on the controller so
    `spur k8s up` assigns roles without a real k0s bring-up."""
    unstarted_cluster.start(config_overrides={"cluster": {"enabled": True}})
    yield unstarted_cluster


@pytest.mark.k0s
class TestK8sSchedulingExclusion:
    def test_k8s_reserved_node_pends_then_reschedules_after_down(self, k8s_enabled_cluster):
        cluster = k8s_enabled_cluster

        # Baseline: a job runs before any k8s claim.
        script = cluster.write_file("k8s-sched.sh", "#!/bin/bash\necho ok\n")
        base = parse_job_id(cluster.sbatch(["-J", "pre-k8s", "-o",
                                            f"{cluster.remote_dir}/pre.out", script]))
        assert base is not None
        wait_job(cluster, base, timeout=90)

        # Claim the node(s) for k8s -> the scheduler gate should exclude them.
        cluster.k8s_up()
        held = _wait_gate_active(cluster, script)

        # The claimed node keeps the job pending, not running.
        assert job_state(cluster.squeue_all(), held) == "PD", (
            f"job should stay pending on a k8s-reserved node:\n{cluster.squeue_all()}"
        )

        # Teardown clears the roles -> the pending job schedules and completes.
        cluster.k8s_down(reset=True)
        wait_job(cluster, held, timeout=180)
