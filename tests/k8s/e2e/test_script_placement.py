# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for where a batch script runs (``--script-placement``) on Kubernetes nodes."""

import dataclasses
import re

import pytest

from k8s_cluster import (
    ClusterFixture,
    FixtureConfig,
    assert_eventually,
    exec_in_pod,
    multinode_spurjob,
    wait_spurjob_state,
)

BATCH_HOST_K8S_UNSUPPORTED = (
    "batch-host script placement is not supported on K8s virtual agents"
)


@pytest.fixture(scope="class")
def cluster(k8s_suite):
    # Overrides conftest's `cluster`, so its between-test cleanup still applies.
    single = FixtureConfig.single_node()
    cfg = dataclasses.replace(
        single,
        config_toml=single.config_toml
        + '\n\n[controller]\nscript_placement = "batch-host"',
    )
    c = ClusterFixture.deploy(k8s_suite, cfg)
    yield c
    c.teardown_workloads()


def _spur(cluster, command: str) -> str:
    """Run `spur <command>` in the controller Pod; its stderr and exit status join stdout."""
    return exec_in_pod(
        cluster.namespace,
        "spurctld-0",
        ["sh", "-c", f"spur {command} 2>&1; echo exit=$?"],
    )


def _node_names(cluster) -> list[str]:
    out = exec_in_pod(
        cluster.namespace, "spurctld-0", ["spur", "nodes", "-N", "-h", "-o", "%N"]
    )
    return sorted({line.strip() for line in out.splitlines() if line.strip()})


def _all_pod_logs(cluster, name: str) -> str:
    pods = cluster.core_v1.list_namespaced_pod(
        cluster.namespace, label_selector=f"spur.amd.com/job-name={name}"
    )
    return "\n".join(
        cluster.core_v1.read_namespaced_pod_log(pod.metadata.name, cluster.namespace)
        for pod in pods.items
    )


class TestScriptPlacementOnK8s:
    def test_a_spurjob_runs_on_every_node_under_a_batch_host_default(self, cluster):
        # Rank 1 outlives rank 0, so a batch-host run would stop it before it prints.
        job = multinode_spurjob(
            "it-placement",
            [
                "sh",
                "-c",
                "sleep $((SPUR_NODE_RANK * 5)); echo done-rank=$SPUR_NODE_RANK",
            ],
            2,
        )
        cluster.create_spurjob(job)

        wait_spurjob_state(cluster, "it-placement", "Completed", timeout=120)
        logs = _all_pod_logs(cluster, "it-placement")
        assert "done-rank=0" in logs and "done-rank=1" in logs, logs

    def test_an_explicit_batch_host_job_pinned_to_k8s_nodes_is_refused(self, cluster):
        nodes = _node_names(cluster)
        assert len(nodes) >= 2, f"needs two registered nodes, found {nodes}"

        out = _spur(
            cluster,
            "sbatch --script-placement=batch-host --nodes=2 "
            f"--nodelist={nodes[0]},{nodes[1]} --wrap='echo hi'",
        )

        assert BATCH_HOST_K8S_UNSUPPORTED in out, out
        assert "Submitted batch job" not in out, out
        assert not out.rstrip().endswith("exit=0"), out

    def test_an_unpinned_batch_host_job_waits_and_says_why(self, cluster):
        out = _spur(
            cluster, "sbatch --script-placement=batch-host --nodes=2 --wrap='echo hi'"
        )
        submitted = re.search(r"Submitted batch job (\d+)", out)
        assert submitted, out
        job_id = submitted.group(1)

        try:
            assert_eventually(
                60,
                2,
                f"job {job_id} never reported why it cannot run on K8s nodes",
                lambda: BATCH_HOST_K8S_UNSUPPORTED
                in _spur(cluster, f"scontrol show job {job_id}"),
            )
        finally:
            _spur(cluster, f"scancel {job_id}")
