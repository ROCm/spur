# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""SpurJob failure paths.

These cover the ways a pod can fail that are not a plain non-zero exit: the
kernel killing it for memory, the kubelet never starting it, and pods left
behind when the operator restarts. Each must land the SpurJob in a terminal
state rather than leaving it stuck in Running.
"""

import time

from kubernetes.client.exceptions import ApiException

from k8s_cluster import (
    DEFAULT_TIMEOUT,
    assert_eventually,
    list_spurjob_pods,
    resource_spurjob,
    simple_spurjob,
    spurjob_with_spec,
    wait_spurjob_pod,
    wait_spurjob_pods_exist,
    wait_spurjob_state,
)
import pytest

pytestmark = pytest.mark.suite_k8s_core

# `tail` buffers its whole input, so this allocates far past the memory limit.
MEMORY_HOG = "head -c 512m /dev/zero | tail -c 512m > /dev/null"

UNPULLABLE_IMAGE = "ghcr.io/spur-e2e/definitely-not-a-real-image:v0"


def _pod_names(cluster, name: str) -> set[str]:
    return {p.metadata.name for p in list_spurjob_pods(cluster, name)}


def _delete_pods(cluster, name: str) -> None:
    for pod_name in _pod_names(cluster, name):
        try:
            cluster.core_v1.delete_namespaced_pod(
                pod_name, cluster.namespace, grace_period_seconds=0
            )
        except ApiException:
            pass


def _assert_state_holds(cluster, name: str, expected: str, seconds: int = 20) -> None:
    """Assert the SpurJob stays in `expected` for a while.

    Late watch events arrive asynchronously, so a single read right after the
    trigger proves nothing.
    """
    deadline = time.time() + seconds
    while time.time() < deadline:
        state = (cluster.get_spurjob(name).get("status") or {}).get("state")
        assert state == expected, (
            f"SpurJob {name} moved from {expected} to {state}"
        )
        time.sleep(2)


def _container_states(cluster, name: str):
    for pod in list_spurjob_pods(cluster, name):
        for status in pod.status.container_statuses or []:
            if status.state is not None:
                yield status.state


class TestOutOfMemory:
    def test_oom_killed_pod_maps_to_the_oom_state(self, cluster):
        """OOM travels out-of-band through the signal sentinel rather than the
        wire state, so it is worth proving end to end that it survives the trip
        from kubelet through the operator to spurctld."""
        cluster.create_spurjob(
            resource_spurjob(
                "it-oom",
                ["sh", "-c", MEMORY_HOG],
                memory_per_node="64Mi",
            )
        )

        result = wait_spurjob_state(cluster, "it-oom", "OutOfMemory", timeout=180)
        assert (result.get("status") or {}).get("state") == "OutOfMemory"

    def test_container_is_reported_oom_killed_by_kubelet(self, cluster):
        """Guards the mapping above: the pod really was OOMKilled, so the state
        is not arriving from some other failure path."""
        cluster.create_spurjob(
            resource_spurjob(
                "it-oom-reason",
                ["sh", "-c", MEMORY_HOG],
                memory_per_node="64Mi",
            )
        )
        wait_spurjob_pods_exist(cluster, "it-oom-reason", timeout=120)

        def oom_killed() -> bool:
            return any(
                state.terminated is not None
                and state.terminated.reason == "OOMKilled"
                for state in _container_states(cluster, "it-oom-reason")
            )

        assert_eventually(180, 3, "kubelet never reported OOMKilled", oom_killed)

    def test_the_same_workload_completes_under_a_generous_limit(self, cluster):
        cluster.create_spurjob(
            resource_spurjob(
                "it-oom-ok",
                ["sh", "-c", f"{MEMORY_HOG} && echo OOM_OK"],
                memory_per_node="1Gi",
            )
        )
        wait_spurjob_state(cluster, "it-oom-ok", "Completed", timeout=180)


class TestImagePull:
    def test_unpullable_image_fails_the_job(self, cluster):
        """The container never starts, so nothing reports back on its own; the
        operator has to notice the failure from the Pending phase."""
        cluster.create_spurjob(
            spurjob_with_spec("it-badimage", ["sh", "-c", "true"], image=UNPULLABLE_IMAGE)
        )

        result = wait_spurjob_state(cluster, "it-badimage", "Failed", timeout=240)
        assert (result.get("status") or {}).get("state") == "Failed"

    def test_image_pull_failure_is_visible_on_the_pod(self, cluster):
        cluster.create_spurjob(
            spurjob_with_spec(
                "it-badimage-reason", ["sh", "-c", "true"], image=UNPULLABLE_IMAGE
            )
        )
        wait_spurjob_pods_exist(cluster, "it-badimage-reason", timeout=120)

        def pull_error() -> bool:
            return any(
                state.waiting is not None
                and state.waiting.reason in ("ImagePullBackOff", "ErrImagePull")
                for state in _container_states(cluster, "it-badimage-reason")
            )

        assert_eventually(
            240, 3, "kubelet never reported an image pull failure", pull_error
        )

    def test_a_failed_pull_does_not_block_the_next_job(self, cluster):
        cluster.create_spurjob(
            spurjob_with_spec(
                "it-badimage-first", ["sh", "-c", "true"], image=UNPULLABLE_IMAGE
            )
        )
        wait_spurjob_state(cluster, "it-badimage-first", "Failed", timeout=240)

        cluster.create_spurjob(
            simple_spurjob("it-after-badimage", ["sh", "-c", "echo AFTER_OK"])
        )
        wait_spurjob_state(cluster, "it-after-badimage", "Completed")


class TestStaleStatusReports:
    def test_a_completed_job_is_not_reopened_by_a_late_pod_event(self, cluster):
        """Deleting the pod after the job finished produces another watch event
        for the same job id; the controller must not walk the job back out of
        its terminal state."""
        cluster.create_spurjob(simple_spurjob("it-stale", ["sh", "-c", "echo STALE_OK"]))
        wait_spurjob_state(cluster, "it-stale", "Completed")

        _delete_pods(cluster, "it-stale")
        _assert_state_holds(cluster, "it-stale", "Completed")

    def test_a_failed_job_keeps_its_identity_after_a_late_event(self, cluster):
        cluster.create_spurjob(simple_spurjob("it-stale-fail", ["sh", "-c", "exit 3"]))
        before = wait_spurjob_state(cluster, "it-stale-fail", "Failed")
        job_id = (before.get("status") or {}).get("spurJobId")

        _delete_pods(cluster, "it-stale-fail")
        _assert_state_holds(cluster, "it-stale-fail", "Failed")

        after = cluster.get_spurjob("it-stale-fail")
        assert (after.get("status") or {}).get("spurJobId") == job_id, (
            "a late pod event re-submitted the job under a new id"
        )

    def test_repeated_reads_of_a_terminal_job_are_stable(self, cluster):
        """The reconciler stops polling spurctld once a job is terminal; a
        drifting state here would mean it never stopped."""
        cluster.create_spurjob(simple_spurjob("it-terminal", ["sh", "-c", "true"]))
        wait_spurjob_state(cluster, "it-terminal", "Completed")
        _assert_state_holds(cluster, "it-terminal", "Completed", seconds=30)


class TestOrphanPodCleanup:
    def test_terminal_pods_of_a_deleted_job_are_reaped_on_restart(self, cluster):
        """Deleting the SpurJob leaves its finished pod behind with no matching
        job id; the operator sweeps those at startup."""
        cluster.create_spurjob(simple_spurjob("it-orphan", ["sh", "-c", "echo ORPHAN_OK"]))
        wait_spurjob_state(cluster, "it-orphan", "Completed")

        orphan = wait_spurjob_pod(cluster, "it-orphan").metadata.name
        cluster.delete_spurjob("it-orphan")

        cluster.restart_operator()

        def gone() -> bool:
            try:
                cluster.core_v1.read_namespaced_pod(orphan, cluster.namespace)
                return False
            except ApiException as exc:
                return exc.status == 404

        assert_eventually(
            DEFAULT_TIMEOUT, 3, f"orphan pod {orphan} was not cleaned up", gone
        )

    def test_pods_of_a_live_job_survive_an_operator_restart(self, cluster):
        """The sweep keys off the SpurJob list, not pod age — a running job's
        pod must not be collateral."""
        cluster.create_spurjob(simple_spurjob("it-live-restart", ["sleep", "300"]))
        wait_spurjob_pods_exist(cluster, "it-live-restart", timeout=120)
        before = _pod_names(cluster, "it-live-restart")
        assert before, "operator never created a pod"

        cluster.restart_operator()

        after = _pod_names(cluster, "it-live-restart")
        assert before <= after, (
            f"the restart reaped a live job's pods: {sorted(before)} -> {sorted(after)}"
        )
        cluster.delete_spurjob("it-live-restart")

    def test_the_operator_serves_new_jobs_after_a_restart(self, cluster):
        cluster.restart_operator()
        cluster.create_spurjob(
            simple_spurjob("it-post-restart", ["sh", "-c", "echo RESTART_OK"])
        )
        wait_spurjob_state(cluster, "it-post-restart", "Completed", timeout=180)
