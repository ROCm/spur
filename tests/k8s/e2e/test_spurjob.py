# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Single-node SpurJob E2E tests for Kubernetes."""

from k8s_cluster import (
    DEFAULT_TIMEOUT,
    assert_eventually,
    cross_namespace_name,
    delete_namespace,
    ensure_namespace,
    job_service_exists,
    multinode_spurjob,
    read_all_spurjob_pod_logs,
    read_spurjob_pod_logs,
    simple_spurjob,
    spurjob_with_env,
    wait_spurjob_pods_exist,
    wait_spurjob_state,
)
import pytest

pytestmark = pytest.mark.suite_k8s_core


class TestSpurJobLifecycle:
    def test_simple_spurjob_completes(self, cluster):
        job = simple_spurjob(
            "it-simple",
            ["sh", "-c", "echo SPUR_K8S_OK && sleep 1"],
        )
        cluster.create_spurjob(job)

        completed = wait_spurjob_state(cluster, "it-simple", "Completed")
        status = completed.get("status") or {}
        assert status.get("spurJobId") is not None, "should have a Spur job ID"

    def test_env_vars_passed_through(self, cluster):
        job = spurjob_with_env(
            "it-env",
            ["sh", "-c", "echo job=$SPUR_JOB_ID custom=$CUSTOM_VAR"],
            {"CUSTOM_VAR": "spur-ci-test"},
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-env", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-env")
        assert "custom=spur-ci-test" in logs, f"expected CUSTOM_VAR in logs:\n{logs}"
        job_idx = logs.find("job=")
        assert job_idx >= 0, f"expected SPUR_JOB_ID in logs:\n{logs}"
        job_val = logs[job_idx + 4 :].split()[0] if logs[job_idx + 4 :] else ""
        assert job_val, f"expected non-empty SPUR_JOB_ID in logs:\n{logs}"

    def test_job_passes_through_pending_before_running(self, cluster):
        """The operator must surface the pre-Running state, not just terminal ones."""
        job = simple_spurjob("it-pending", ["sh", "-c", "sleep 20"])
        cluster.create_spurjob(job)

        seen: set[str] = set()

        def reached_running() -> bool:
            state = (cluster.get_spurjob("it-pending").get("status") or {}).get("state")
            if state:
                seen.add(state)
            return state == "Running"

        assert_eventually(
            DEFAULT_TIMEOUT, 1, "SpurJob never reached Running", reached_running
        )
        assert "Pending" in seen, (
            f"expected a Pending observation before Running, saw {sorted(seen)}"
        )
        cluster.delete_spurjob("it-pending")

    def test_multinode_job_assigns_nodes(self, cluster):
        job = multinode_spurjob(
            "it-multi",
            [
                "sh",
                "-c",
                "echo rank=$SPUR_NODE_RANK nodes=$SPUR_NNODES host=$(hostname)",
            ],
            2,
        )
        cluster.create_spurjob(job)

        completed = wait_spurjob_state(
            cluster, "it-multi", "Completed", timeout=90
        )
        assigned = (completed.get("status") or {}).get("assignedNodes") or []
        assert assigned, "multi-node job should have assigned nodes"

    def test_multinode_ranks_and_master_addr_are_distinct(self, cluster):
        """Each pod must get its own rank and a shared rendezvous address."""
        job = multinode_spurjob(
            "it-multi-env",
            [
                "sh",
                "-c",
                "echo rank=$SPUR_NODE_RANK nnodes=$SPUR_NNODES "
                "master=$MASTER_ADDR world=$WORLD_SIZE",
            ],
            2,
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-multi-env", "Completed", timeout=90)

        logs = read_all_spurjob_pod_logs(cluster, "it-multi-env")
        assert len(logs) == 2, f"expected 2 pods, got {sorted(logs)}"

        def field(text: str, key: str) -> str:
            for token in text.split():
                if token.startswith(f"{key}="):
                    return token.split("=", 1)[1]
            return ""

        ranks = {field(text, "rank") for text in logs.values()}
        assert ranks == {"0", "1"}, f"expected ranks 0 and 1, got {ranks}:\n{logs}"

        masters = {field(text, "master") for text in logs.values()}
        assert len(masters) == 1 and masters != {""}, (
            f"every pod must share one MASTER_ADDR, got {masters}:\n{logs}"
        )

        for text in logs.values():
            assert field(text, "nnodes") == "2", f"SPUR_NNODES must be 2:\n{text}"
            assert field(text, "world") == "2", f"WORLD_SIZE must be 2:\n{text}"

    def test_headless_service_is_removed_on_cancel(self, cluster):
        """Cancelling a multi-node job must clean up its Service, not just pods."""
        job = multinode_spurjob("it-multi-svc", ["sleep", "600"], 2)
        cluster.create_spurjob(job)

        wait_spurjob_pods_exist(cluster, "it-multi-svc", timeout=90)
        spur_job_id = (
            cluster.get_spurjob("it-multi-svc").get("status") or {}
        ).get("spurJobId")
        assert spur_job_id is not None, "multi-node job never got a Spur job ID"

        assert_eventually(
            DEFAULT_TIMEOUT,
            2,
            f"headless Service spur-job-{spur_job_id} was never created",
            lambda: job_service_exists(cluster, spur_job_id),
        )

        cluster.delete_spurjob("it-multi-svc")

        assert_eventually(
            DEFAULT_TIMEOUT,
            2,
            f"headless Service spur-job-{spur_job_id} outlived the cancelled job",
            lambda: not job_service_exists(cluster, spur_job_id),
        )

    def test_cancellation_cleans_up_pods(self, cluster):
        job = simple_spurjob("it-cancel", ["sleep", "600"])
        cluster.create_spurjob(job)

        wait_spurjob_pods_exist(cluster, "it-cancel")
        cluster.delete_spurjob("it-cancel")

        assert_eventually(
            DEFAULT_TIMEOUT,
            2,
            "pods not cleaned up after SpurJob cancellation",
            lambda: len(
                cluster.core_v1.list_namespaced_pod(
                    cluster.namespace,
                    label_selector="spur.amd.com/job-name=it-cancel",
                ).items
            )
            == 0,
        )

    def test_failure_detected(self, cluster):
        job = simple_spurjob("it-fail", ["sh", "-c", "exit 42"])
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-fail", "Failed")

    def test_sequential_jobs_all_complete(self, cluster):
        for i in range(1, 4):
            name = f"it-seq-{i}"
            job = simple_spurjob(name, ["sh", "-c", f"echo seq={i}"])
            cluster.create_spurjob(job)
            wait_spurjob_state(cluster, name, "Completed")

    def test_cross_namespace_no_pod_leakage(self, cluster):
        cross_ns = cross_namespace_name(cluster.namespace)

        ensure_namespace(cross_ns)
        try:
            job = simple_spurjob(
                "it-cross-ns",
                ["sh", "-c", "echo CROSS_NS_OK && sleep 1"],
            )
            job["metadata"]["namespace"] = cross_ns
            cluster.custom_api.create_namespaced_custom_object(
                group="spur.amd.com",
                version="v1alpha1",
                namespace=cross_ns,
                plural="spurjobs",
                body=job,
            )

            completed = wait_spurjob_state(
                cluster, "it-cross-ns", "Completed", namespace=cross_ns
            )
            job_id = (completed.get("status") or {}).get("spurJobId")
            assert job_id is not None, "cross-ns job completed without spurJobId"
            leaked = cluster.core_v1.list_namespaced_pod(
                cluster.namespace,
                label_selector=f"spur.amd.com/job-id={job_id}",
            )
            assert not leaked.items, (
                f"pods leaked into spur namespace for cross-ns job "
                f"(found {len(leaked.items)})"
            )
        finally:
            delete_namespace(cross_ns, wait=True)
