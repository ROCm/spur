# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Single-node SpurJob E2E tests for Kubernetes."""

from k8s_cluster import (
    DEFAULT_TIMEOUT,
    assert_eventually,
    cross_namespace_name,
    delete_namespace,
    ensure_namespace,
    exec_in_pod,
    multinode_spurjob,
    read_spurjob_pod_logs,
    simple_spurjob,
    spurjob_with_env,
    wait_spurjob_pods_exist,
    wait_spurjob_state,
)


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
        job_val = logs[job_idx + 4:].split()[0] if logs[job_idx + 4:] else ""
        assert job_val, f"expected non-empty SPUR_JOB_ID in logs:\n{logs}"

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

    def test_scancel_of_a_multinode_job_releases_every_node(self, cluster):
        # The CLI presents no credential, so only the job's owner may cancel it.
        user = exec_in_pod(cluster.namespace, "spurctld-0", ["id", "-un"], check=True)
        job = multinode_spurjob("it-scancel", ["sleep", "600"], 2)
        job["metadata"]["annotations"] = {"spur.amd.com/user": user.strip()}
        cluster.create_spurjob(job)
        running = wait_spurjob_state(cluster, "it-scancel", "Running", timeout=90)
        job_id = (running.get("status") or {}).get("spurJobId")
        assert job_id is not None, "a running SpurJob has a Spur job ID"

        exec_in_pod(
            cluster.namespace, "spurctld-0", ["spur", "scancel", str(job_id)], check=True
        )

        # Unreported nodes are drained once complete_wait_secs (300 s) runs out.
        wait_spurjob_state(cluster, "it-scancel", "Cancelled", timeout=90)
        # As PID 1, sleep ignores SIGTERM, so each Pod runs out its grace period.
        # A node may be released only once its Pod has stopped.
        pods = cluster.core_v1.list_namespaced_pod(
            cluster.namespace, label_selector=f"spur.amd.com/job-id={job_id}"
        ).items
        unstopped = [
            p.metadata.name
            for p in pods
            if p.status.phase not in ("Succeeded", "Failed")
        ]
        assert not unstopped, f"the job ended while these Pods still ran: {unstopped}"
        states = exec_in_pod(
            cluster.namespace,
            "spurctld-0",
            ["spur", "nodes", "-N", "-h", "-o", "%N %t"],
            check=True,
        )
        assert "drain" not in states, f"no node may be drained:\n{states}"

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
