# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""SpurJob CRD spec field coverage.

test_spurjob.py exercises the job lifecycle; this module checks that the spec
fields a user writes actually reach the created Pod. Assertions go through
assert_pod_spec, which reads the real PodSpec rather than inferring from logs.
"""

import pytest
from kubernetes.client.exceptions import ApiException

from k8s_cluster import (
    assert_pod_spec,
    read_spurjob_pod_logs,
    resource_spurjob,
    secret_env_spurjob,
    security_spurjob,
    spurjob_with_spec,
    volume_spurjob,
    wait_spurjob_pod,
    wait_spurjob_state,
)

CONTAINER = "spec.containers.0"


def _mount_paths(pod) -> dict[str, str]:
    """Mount path -> volume name for the job container."""
    return {m.mount_path: m.name for m in (pod.spec.containers[0].volume_mounts or [])}


def _volumes(pod) -> dict:
    return {v.name: v for v in (pod.spec.volumes or [])}


class TestVolumes:
    def test_host_path_volume_is_mounted(self, cluster):
        job = volume_spurjob(
            "it-vol-host", ["sh", "-c", "ls -d /mnt/host"], ["/tmp:/mnt/host"]
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-vol-host")
        mounts = _mount_paths(pod)
        assert "/mnt/host" in mounts, f"hostPath was not mounted: {mounts}"

        volume = _volumes(pod)[mounts["/mnt/host"]]
        assert volume.host_path is not None, f"expected a hostPath volume: {volume}"
        assert volume.host_path.path == "/tmp"
        assert volume.host_path.type == "DirectoryOrCreate", (
            "the operator must create the host directory rather than failing "
            f"the pod, got {volume.host_path.type}"
        )

    def test_read_only_suffix_is_honored(self, cluster):
        job = volume_spurjob(
            "it-vol-ro", ["sh", "-c", "true"], ["/tmp:/mnt/ro:ro"]
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-vol-ro")
        mount = next(
            m for m in pod.spec.containers[0].volume_mounts if m.mount_path == "/mnt/ro"
        )
        assert mount.read_only is True, f"the :ro suffix was dropped: {mount}"

    def test_multiple_volumes_get_distinct_names(self, cluster):
        job = volume_spurjob(
            "it-vol-multi",
            ["sh", "-c", "true"],
            ["/tmp:/mnt/a", "/var/tmp:/mnt/b"],
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-vol-multi")
        mounts = _mount_paths(pod)
        assert {"/mnt/a", "/mnt/b"} <= set(mounts), f"expected both mounts: {mounts}"
        assert mounts["/mnt/a"] != mounts["/mnt/b"], (
            f"volumes must not collide on one name: {mounts}"
        )

    def test_host_path_contents_are_visible_to_the_job(self, cluster):
        job = volume_spurjob(
            "it-vol-read",
            ["sh", "-c", "test -d /mnt/host && echo VOLUME_OK"],
            ["/tmp:/mnt/host"],
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-vol-read", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-vol-read")
        assert "VOLUME_OK" in logs, f"the mount was not usable:\n{logs}"

    def test_malformed_volume_is_ignored(self, cluster):
        """A single-component entry has no target path, so it is dropped rather
        than producing an unschedulable pod."""
        job = volume_spurjob("it-vol-bad", ["sh", "-c", "echo BAD_VOL_OK"], ["/tmp"])
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-vol-bad", "Completed")

        pod = wait_spurjob_pod(cluster, "it-vol-bad")
        assert "/tmp" not in _mount_paths(pod), (
            "a malformed volume entry must not be mounted"
        )


class TestSecretEnv:
    @pytest.fixture
    def secret(self, cluster):
        name = "it-secret"
        body = {
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": name},
            "stringData": {"token": "s3cr3t-value"},
        }
        try:
            cluster.core_v1.create_namespaced_secret(cluster.namespace, body)
        except ApiException as exc:
            if exc.status != 409:
                raise
        yield name
        try:
            cluster.core_v1.delete_namespaced_secret(name, cluster.namespace)
        except ApiException:
            pass

    def test_secret_env_becomes_a_secret_key_ref(self, cluster, secret):
        job = secret_env_spurjob(
            "it-secret-ref",
            ["sh", "-c", "echo token=$MY_TOKEN"],
            {"MY_TOKEN": f"{secret}/token"},
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-secret-ref")
        env = {e.name: e for e in pod.spec.containers[0].env or []}
        assert "MY_TOKEN" in env, f"secretEnv did not reach the pod: {sorted(env)}"
        ref = env["MY_TOKEN"].value_from.secret_key_ref
        assert (ref.name, ref.key) == (secret, "token"), f"wrong secret ref: {ref}"
        assert env["MY_TOKEN"].value is None, (
            "the secret value must not be inlined into the PodSpec"
        )

    def test_secret_value_reaches_the_job(self, cluster, secret):
        job = secret_env_spurjob(
            "it-secret-val",
            ["sh", "-c", "echo token=$MY_TOKEN"],
            {"MY_TOKEN": f"{secret}/token"},
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-secret-val", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-secret-val")
        assert "token=s3cr3t-value" in logs, f"secret was not injected:\n{logs}"

    def test_missing_secret_is_optional(self, cluster):
        """The reference is marked optional, so an absent Secret leaves the var
        empty instead of wedging the pod."""
        job = secret_env_spurjob(
            "it-secret-missing",
            ["sh", "-c", "echo token=[$MY_TOKEN] && echo MISSING_OK"],
            {"MY_TOKEN": "no-such-secret/token"},
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-secret-missing", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-secret-missing")
        assert "token=[]" in logs, f"expected an empty value:\n{logs}"

    def test_malformed_secret_ref_is_ignored(self, cluster):
        job = secret_env_spurjob(
            "it-secret-bad", ["sh", "-c", "echo BAD_SECRET_OK"], {"MY_TOKEN": "nokey"}
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-secret-bad")
        names = {e.name for e in pod.spec.containers[0].env or []}
        assert "MY_TOKEN" not in names, (
            "a reference without a key must be dropped, not passed through"
        )


class TestSecurityContext:
    def test_host_network_reaches_the_pod(self, cluster):
        job = security_spurjob(
            "it-hostnet", ["sh", "-c", "sleep 5"], host_network=True
        )
        cluster.create_spurjob(job)
        assert_pod_spec(cluster, "it-hostnet", {"spec.host_network": True})

    def test_host_ipc_reaches_the_pod(self, cluster):
        job = security_spurjob("it-hostipc", ["sh", "-c", "sleep 5"], host_ipc=True)
        cluster.create_spurjob(job)
        assert_pod_spec(cluster, "it-hostipc", {"spec.host_ipc": True})

    def test_privileged_reaches_the_container(self, cluster):
        job = security_spurjob("it-priv", ["sh", "-c", "sleep 5"], privileged=True)
        cluster.create_spurjob(job)
        assert_pod_spec(
            cluster, "it-priv", {f"{CONTAINER}.security_context.privileged": True}
        )

    def test_defaults_leave_the_pod_unprivileged(self, cluster):
        job = security_spurjob("it-nopriv", ["sh", "-c", "echo NOPRIV_OK"])
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-nopriv")
        assert pod.spec.host_network in (None, False), (
            "host networking must be opt-in"
        )
        assert pod.spec.host_ipc in (None, False), "host IPC must be opt-in"
        ctx = pod.spec.containers[0].security_context
        assert ctx is None or ctx.privileged in (None, False), (
            f"privileged must be opt-in, got {ctx}"
        )

    def test_shm_size_creates_a_memory_backed_dev_shm(self, cluster):
        """Multi-GPU collectives need a larger /dev/shm than the 64 MiB default."""
        job = security_spurjob(
            "it-shm", ["sh", "-c", "sleep 5"], shm_size="256Mi"
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-shm")
        assert _mount_paths(pod).get("/dev/shm") == "dshm", (
            f"/dev/shm was not mounted: {_mount_paths(pod)}"
        )
        volume = _volumes(pod)["dshm"]
        assert volume.empty_dir.medium == "Memory", (
            f"/dev/shm must be tmpfs-backed, got {volume.empty_dir}"
        )
        assert volume.empty_dir.size_limit == "256Mi"

    def test_no_shm_size_leaves_dev_shm_alone(self, cluster):
        job = security_spurjob("it-noshm", ["sh", "-c", "echo NOSHM_OK"])
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-noshm")
        assert "/dev/shm" not in _mount_paths(pod), (
            "the operator must not mount /dev/shm unless asked"
        )


class TestResources:
    def test_cpu_request_scales_with_tasks_and_cpus_per_task(self, cluster):
        job = resource_spurjob(
            "it-res-cpu",
            ["sh", "-c", "sleep 5"],
            tasks_per_node=2,
            cpus_per_task=2,
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-res-cpu")
        requests = pod.spec.containers[0].resources.requests or {}
        limits = pod.spec.containers[0].resources.limits or {}
        assert requests.get("cpu") == "4", (
            f"cpu request should be tasksPerNode * cpusPerTask, got {requests}"
        )
        assert limits.get("cpu") == requests.get("cpu"), (
            f"cpu request and limit must match, got {limits} vs {requests}"
        )

    def test_memory_per_node_reaches_pod_resources(self, cluster):
        job = resource_spurjob(
            "it-res-mem", ["sh", "-c", "sleep 5"], memory_per_node="256Mi"
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-res-mem")
        requests = pod.spec.containers[0].resources.requests or {}
        assert requests.get("memory"), f"no memory request on the pod: {requests}"
        assert requests["memory"].endswith("Mi"), (
            f"memory should be expressed in MiB, got {requests['memory']}"
        )

    def test_extra_resources_appear_in_requests_and_limits(self, cluster):
        job = resource_spurjob(
            "it-res-extra",
            ["sh", "-c", "sleep 5"],
            extra_resources={"hugepages-2Mi": "64Mi"},
        )
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-res-extra")
        requests = pod.spec.containers[0].resources.requests or {}
        limits = pod.spec.containers[0].resources.limits or {}
        assert requests.get("hugepages-2Mi") == "64Mi", (
            f"extraResources missing from requests: {requests}"
        )
        assert limits.get("hugepages-2Mi") == "64Mi", (
            f"extraResources missing from limits: {limits}"
        )


class TestCommandAndImage:
    def test_args_are_appended_to_command(self, cluster):
        job = spurjob_with_spec(
            "it-args", ["echo"], args=["ARGS_APPENDED_OK"]
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-args", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-args")
        assert "ARGS_APPENDED_OK" in logs, f"args were dropped:\n{logs}"

    def test_args_alone_become_the_command(self, cluster):
        job = spurjob_with_spec(
            "it-args-only", [], args=["echo", "ARGS_ONLY_OK"]
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-args-only", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-args-only")
        assert "ARGS_ONLY_OK" in logs, f"args-only job did not run:\n{logs}"

    def test_custom_image_is_used(self, cluster):
        job = spurjob_with_spec(
            "it-image", ["sh", "-c", "echo IMAGE_OK"], image="busybox:1.36"
        )
        cluster.create_spurjob(job)
        assert_pod_spec(cluster, "it-image", {f"{CONTAINER}.image": "busybox:1.36"})

    def test_pod_carries_the_operator_labels(self, cluster):
        job = spurjob_with_spec("it-labels", ["sh", "-c", "sleep 5"])
        cluster.create_spurjob(job)

        pod = wait_spurjob_pod(cluster, "it-labels")
        labels = pod.metadata.labels or {}
        assert labels.get("spur.amd.com/managed-by") == "spur-k8s-operator"
        assert labels.get("spur.amd.com/job-name") == "it-labels"
        assert labels.get("spur.amd.com/job-id"), (
            f"the pod must carry its Spur job id: {labels}"
        )
        assert pod.spec.containers[0].name == "spur-job"


class TestSchedulingFields:
    def test_partition_and_account_reach_the_job_environment(self, cluster):
        job = spurjob_with_spec(
            "it-part-acct",
            ["sh", "-c", "echo part=$SPUR_JOB_PARTITION acct=$SPUR_JOB_ACCOUNT"],
            partition="default",
            account="root",
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-part-acct", "Completed")
        logs = read_spurjob_pod_logs(cluster, "it-part-acct")
        assert "part=default" in logs, f"partition was not propagated:\n{logs}"
        assert "acct=root" in logs, f"account was not propagated:\n{logs}"

    def test_unknown_partition_fails_the_job(self, cluster):
        job = spurjob_with_spec(
            "it-bad-part", ["sh", "-c", "true"], partition="no-such-partition"
        )
        cluster.create_spurjob(job)

        state = wait_spurjob_state(
            cluster, "it-bad-part", "Failed", timeout=90
        )
        assert (state.get("status") or {}).get("state") == "Failed"

    def test_time_limit_is_accepted_and_enforced(self, cluster):
        """timeLimit shapes the Spur job, not the PodSpec, so it is observed
        through the job outcome."""
        job = spurjob_with_spec(
            "it-timelimit", ["sh", "-c", "sleep 600"], timeLimit="1m"
        )
        cluster.create_spurjob(job)

        result = wait_spurjob_state(cluster, "it-timelimit", "Timeout", timeout=180)
        assert (result.get("status") or {}).get("state") == "Timeout"

    def test_generous_time_limit_lets_the_job_finish(self, cluster):
        job = spurjob_with_spec(
            "it-timelimit-ok", ["sh", "-c", "echo TIMELIMIT_OK"], timeLimit="1h"
        )
        cluster.create_spurjob(job)
        wait_spurjob_state(cluster, "it-timelimit-ok", "Completed")
        assert "TIMELIMIT_OK" in read_spurjob_pod_logs(cluster, "it-timelimit-ok")
