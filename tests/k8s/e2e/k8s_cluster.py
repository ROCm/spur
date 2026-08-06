# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Kubernetes cluster management for Spur E2E tests."""

from __future__ import annotations

import base64
import logging
import os
import shlex
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

import yaml
from kubernetes import client, config
from kubernetes.client.rest import ApiException
from kubernetes.stream import stream
from kubernetes.utils import create_from_dict

logger = logging.getLogger(__name__)

_MANIFESTS_DIR = Path(__file__).resolve().parent / "manifests"
_DEFAULT_IMAGE = "ghcr.io/rocm/spur:ci"

SPUR_JOB_GROUP = "spur.amd.com"
SPUR_JOB_VERSION = "v1alpha1"
SPUR_JOB_PLURAL = "spurjobs"
DEFAULT_TIMEOUT = 60
WAIT_INTERVAL = 2
HA_TIMEOUT = 90
CLUSTER_SCOPED_KINDS = frozenset({"ClusterRole", "ClusterRoleBinding"})


def wait_until(
    check_fn: Callable[[], bool],
    timeout: int,
    msg: str,
    interval: int = WAIT_INTERVAL,
) -> None:
    """Poll until check_fn returns True or timeout expires."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if check_fn():
            return
        time.sleep(interval)
    raise TimeoutError(f"timed out after {timeout}s: {msg}")


def spur_namespace() -> str:
    if ns := os.environ.get("SPUR_TEST_NS"):
        return ns
    if run_id := os.environ.get("GITHUB_RUN_ID"):
        return f"spur-test-{run_id}"
    return f"spur-ci-{os.getpid()}-{int(time.time())}"


def cross_namespace_name(primary_namespace: str) -> str:
    return f"{primary_namespace}-user1"


def _load_kube_config() -> None:
    try:
        config.load_kube_config()
    except config.ConfigException:
        config.load_incluster_config()


def verify_kube_access() -> None:
    """Fail fast with a clear message when the cluster API is unreachable."""
    import pytest

    try:
        _load_kube_config()
    except config.ConfigException as exc:
        pytest.exit(
            f"kubeconfig not available — cannot run K8s E2E tests: {exc}",
            returncode=1,
        )
    try:
        client.CoreV1Api().list_namespace(limit=1)
    except ApiException as exc:
        pytest.exit(
            f"Kubernetes API unreachable — cannot run K8s E2E tests: {exc}",
            returncode=1,
        )


def _is_not_found(exc: ApiException) -> bool:
    return exc.status == 404


def _is_already_exists(exc: ApiException) -> bool:
    return exc.status == 409


def patch_namespace_in_value(value: dict, namespace: str) -> dict:
    if namespace == "spur":
        return value
    import json
    import re

    json_str = json.dumps(value)
    patched = re.sub(
        r'"namespace"\s*:\s*"spur"',
        f'"namespace": "{namespace}"',
        json_str,
    ).replace(
        ".spur.svc.cluster.local",
        f".{namespace}.svc.cluster.local",
    )
    return json.loads(patched)


@dataclass
class FixtureConfig:
    replicas: int
    image: str
    config_toml: str
    extra_operator_args: list[str] = field(default_factory=list)
    needs_postgres: bool = False

    @classmethod
    def single_node(cls) -> FixtureConfig:
        image = os.environ.get("SPUR_CI_IMAGE", _DEFAULT_IMAGE)
        return cls(
            replicas=1,
            image=image,
            config_toml="""
cluster_name = "k8s-ci"

[scheduler]
interval_secs = 1
plugin = "backfill"

[metrics]
bind = "all"

[[partitions]]
name = "default"
state = "UP"
default = true
nodes = "ALL"
max_time = "1h"
default_time = "10m"
""".strip(),
        )

    @classmethod
    def with_quota(cls) -> FixtureConfig:
        """Single controller with the operator's quota controller switched on.

        Quota projection reads accounts over the SlurmAccounting RPC, which
        spurctld only serves when accounting is backed by a database, so this
        variant also brings up an ephemeral Postgres.
        """
        cfg = cls.single_node()
        cfg.extra_operator_args = ["--enable-quota"]
        cfg.needs_postgres = True
        cfg.config_toml += (
            "\n\n[accounting]\n"
            'database_url = "postgresql://spur:spur@spur-postgres:5432/spur"\n'
        )
        return cfg

    @classmethod
    def raft_ha(cls) -> FixtureConfig:
        image = os.environ.get("SPUR_CI_IMAGE", _DEFAULT_IMAGE)
        return cls(
            replicas=3,
            image=image,
            config_toml="""
cluster_name = "k8s-ci-raft"

[controller]
peers = [
  "spurctld-0.spurctld.spur.svc.cluster.local:6821",
  "spurctld-1.spurctld.spur.svc.cluster.local:6821",
  "spurctld-2.spurctld.spur.svc.cluster.local:6821",
]

[scheduler]
interval_secs = 1
plugin = "backfill"

[[partitions]]
name = "default"
state = "UP"
default = true
nodes = "ALL"
max_time = "1h"
default_time = "10m"
""".strip(),
        )


class SuiteContext:
    def __init__(self, namespace: str):
        self.namespace = namespace
        self.core_v1 = client.CoreV1Api()
        self.ext_v1 = client.ApiextensionsV1Api()

    @classmethod
    def setup(cls) -> SuiteContext:
        verify_kube_access()
        namespace = spur_namespace()
        ctx = cls(namespace)
        ctx.apply_namespace()
        ctx.apply_crd()
        return ctx

    def teardown(self) -> None:
        # Delete the cross-ns namespace first while the operator in the primary
        # namespace is still running, so SpurJob finalizers can be cleared.
        delete_namespace(cross_namespace_name(self.namespace), wait=True)

        try:
            self.core_v1.delete_namespace(self.namespace)
        except ApiException as exc:
            if not _is_not_found(exc):
                raise

        try:
            self.ext_v1.delete_custom_resource_definition("spurjobs.spur.amd.com")
        except ApiException as exc:
            if not _is_not_found(exc):
                raise

        logger.info("suite teardown complete for namespace %s", self.namespace)

    def apply_namespace(self) -> None:
        body = client.V1Namespace(
            metadata=client.V1ObjectMeta(
                name=self.namespace,
                labels={"app.kubernetes.io/part-of": "spur"},
            )
        )
        try:
            self.core_v1.create_namespace(body)
        except ApiException as exc:
            if not _is_already_exists(exc):
                raise
        logger.info("namespace ensured: %s", self.namespace)

    def apply_crd(self) -> None:
        crd_path = _MANIFESTS_DIR / "spurjob-crd.yaml"
        with crd_path.open() as f:
            crd_body = yaml.safe_load(f)
        try:
            self.ext_v1.create_custom_resource_definition(crd_body)
        except ApiException as exc:
            if not _is_already_exists(exc):
                raise
            self.ext_v1.replace_custom_resource_definition(
                "spurjobs.spur.amd.com", crd_body
            )
        logger.info("SpurJob CRD applied")


class ClusterFixture:
    def __init__(self, suite: SuiteContext, cfg: FixtureConfig):
        _load_kube_config()
        self.suite = suite
        self.namespace = suite.namespace
        self.config = cfg
        self.core_v1 = client.CoreV1Api()
        self.apps_v1 = client.AppsV1Api()
        self.custom_api = client.CustomObjectsApi()

    @classmethod
    def deploy(cls, suite: SuiteContext, cfg: FixtureConfig) -> ClusterFixture:
        fixture = cls(suite, cfg)
        fixture.teardown_workloads()
        fixture.apply_rbac()
        fixture.apply_configmap()
        if cfg.needs_postgres:
            # spurctld only builds its accounting pool at startup, so the
            # database has to be serving before the controller comes up.
            fixture.apply_postgres()
            fixture.wait_postgres_ready()
        fixture.apply_controller()
        fixture.apply_operator()
        fixture.wait_ready()
        return fixture

    def apply_configmap(self) -> None:
        config_toml = self.config.config_toml.replace(
            ".spur.svc.cluster.local",
            f".{self.namespace}.svc.cluster.local",
        )
        body = client.V1ConfigMap(
            metadata=client.V1ObjectMeta(
                name="spur-config",
                namespace=self.namespace,
            ),
            data={"spur.conf": config_toml},
        )
        try:
            self.core_v1.create_namespaced_config_map(self.namespace, body)
        except ApiException as exc:
            if not _is_already_exists(exc):
                raise
            self.core_v1.replace_namespaced_config_map(
                "spur-config", self.namespace, body
            )
        logger.info("ConfigMap spur-config applied")

    def _force_delete_pods(self, label_selector: str) -> None:
        pods = self.core_v1.list_namespaced_pod(
            self.namespace, label_selector=label_selector
        )
        for pod in pods.items:
            name = pod.metadata.name
            try:
                self.core_v1.delete_namespaced_pod(
                    name,
                    self.namespace,
                    grace_period_seconds=0,
                    propagation_policy="Background",
                )
            except ApiException as exc:
                if not _is_not_found(exc):
                    raise

    def _wait_pods_gone(
        self,
        label_selector: str,
        timeout: int = 60,
        *,
        raise_on_timeout: bool = True,
    ) -> None:
        try:
            wait_until(
                lambda: not self.core_v1.list_namespaced_pod(
                    self.namespace, label_selector=label_selector
                ).items,
                timeout,
                f"pods still present for selector {label_selector!r}",
            )
        except TimeoutError:
            if raise_on_timeout:
                raise
            remaining = self.core_v1.list_namespaced_pod(
                self.namespace, label_selector=label_selector
            ).items
            logger.warning(
                "timed out waiting for pods to terminate (selector=%r, remaining=%s)",
                label_selector,
                [p.metadata.name for p in remaining],
            )

    def _delete_resource(self, kind: str, name: str) -> None:
        delete_kwargs = {
            "grace_period_seconds": 0,
            "propagation_policy": "Background",
        }
        try:
            if kind == "Service":
                self.core_v1.delete_namespaced_service(name, self.namespace)
            elif kind == "StatefulSet":
                self.apps_v1.delete_namespaced_stateful_set(
                    name, self.namespace, **delete_kwargs
                )
            elif kind == "Deployment":
                self.apps_v1.delete_namespaced_deployment(
                    name, self.namespace, **delete_kwargs
                )
        except ApiException as exc:
            if not _is_not_found(exc):
                raise

    def _apply_yaml_docs(
        self,
        yaml_path: Path,
        replicas: int | None,
        mutate: Callable[[dict], None] | None = None,
    ) -> None:
        content = yaml_path.read_text()
        api_client = client.ApiClient()
        for doc in content.split("\n---"):
            doc = doc.strip()
            if not doc:
                continue
            patched = doc.replace(_DEFAULT_IMAGE, self.config.image)
            if replicas is not None:
                patched = patched.replace("replicas: 3", f"replicas: {replicas}")
            value = yaml.safe_load(patched)
            value = patch_namespace_in_value(value, self.namespace)
            value.setdefault("metadata", {})["namespace"] = self.namespace
            if mutate is not None:
                mutate(value)
            kind = value.get("kind", "")
            name = value["metadata"]["name"]
            self._delete_resource(kind, name)
            create_from_dict(api_client, value, namespace=self.namespace)

    def apply_rbac(self) -> None:
        content = (_MANIFESTS_DIR / "rbac.yaml").read_text()
        api_client = client.ApiClient()
        rbac_api = client.RbacAuthorizationV1Api()
        for doc in content.split("\n---"):
            doc = doc.strip()
            if not doc:
                continue
            value = patch_namespace_in_value(yaml.safe_load(doc), self.namespace)
            kind = value.get("kind", "")
            name = value["metadata"]["name"]
            if kind not in CLUSTER_SCOPED_KINDS:
                value.setdefault("metadata", {})["namespace"] = self.namespace
            try:
                if kind == "ClusterRole":
                    rbac_api.delete_cluster_role(name)
                elif kind == "ClusterRoleBinding":
                    rbac_api.delete_cluster_role_binding(name)
            except ApiException as exc:
                if not _is_not_found(exc):
                    raise
            if kind in CLUSTER_SCOPED_KINDS:
                create_from_dict(api_client, value)
            else:
                create_from_dict(api_client, value, namespace=self.namespace)
        logger.info("RBAC applied for namespace %s", self.namespace)

    def apply_controller(self) -> None:
        self._apply_yaml_docs(_MANIFESTS_DIR / "spurctld.yaml", self.config.replicas)
        logger.info(
            "controller StatefulSet applied (replicas=%s, image=%s)",
            self.config.replicas,
            self.config.image,
        )

    def apply_operator(self) -> None:
        def add_extra_args(value: dict) -> None:
            if value.get("kind") != "Deployment":
                return
            for container in value["spec"]["template"]["spec"]["containers"]:
                if container["name"] == "operator":
                    container.setdefault("args", []).extend(
                        self.config.extra_operator_args
                    )

        mutate = add_extra_args if self.config.extra_operator_args else None
        self._apply_yaml_docs(_MANIFESTS_DIR / "operator.yaml", None, mutate)
        logger.info(
            "operator Deployment applied (image=%s, extra_args=%s)",
            self.config.image,
            self.config.extra_operator_args,
        )

    def apply_postgres(self) -> None:
        self._apply_yaml_docs(_MANIFESTS_DIR / "postgres.yaml", None)
        logger.info("postgres Deployment applied")

    def wait_postgres_ready(self, timeout: int = 180) -> None:
        wait_until(
            lambda: count_ready_pods(self.namespace, "app=spur-postgres") >= 1,
            timeout,
            "postgres pod not ready",
            interval=3,
        )

    def postgres_available(self) -> bool:
        return count_ready_pods(self.namespace, "app=spur-postgres") >= 1

    def wait_ready(self, timeout: int = 120) -> None:
        wait_until(
            lambda: self._operator_available(),
            timeout,
            "operator deployment not ready",
            interval=3,
        )

        expected = self.config.replicas
        wait_until(
            lambda: self._ready_controller_count() >= expected,
            timeout,
            f"expected {expected} ready controller pod(s)",
            interval=3,
        )
        logger.info("controller pod(s) ready: %s", self._ready_controller_count())

    def _operator_available(self) -> bool:
        try:
            dep = self.apps_v1.read_namespaced_deployment(
                "spur-k8s-operator", self.namespace
            )
            return (dep.status.available_replicas or 0) >= 1
        except ApiException:
            return False

    def _ready_controller_count(self) -> int:
        return count_ready_pods(self.namespace, "app=spurctld")

    def teardown_workloads(self) -> None:
        self._force_delete_pods("app=spurctld")
        self._force_delete_pods("app=spur-k8s-operator")
        self._force_delete_pods("app=spur-postgres")

        for delete_fn in [
            lambda: self.apps_v1.delete_namespaced_deployment(
                "spur-postgres",
                self.namespace,
                grace_period_seconds=0,
                propagation_policy="Background",
            ),
            lambda: self.core_v1.delete_namespaced_service(
                "spur-postgres", self.namespace
            ),
            lambda: self.apps_v1.delete_namespaced_deployment(
                "spur-k8s-operator",
                self.namespace,
                grace_period_seconds=0,
                propagation_policy="Background",
            ),
            lambda: self.apps_v1.delete_namespaced_stateful_set(
                "spurctld",
                self.namespace,
                grace_period_seconds=0,
                propagation_policy="Background",
            ),
            lambda: self.core_v1.delete_namespaced_service("spurctld", self.namespace),
            lambda: self.core_v1.delete_namespaced_service(
                "spur-k8s-operator", self.namespace
            ),
            lambda: self.core_v1.delete_namespaced_config_map(
                "spur-config", self.namespace
            ),
        ]:
            try:
                delete_fn()
            except ApiException as exc:
                if not _is_not_found(exc):
                    raise

        for i in range(3):
            try:
                self.core_v1.delete_namespaced_persistent_volume_claim(
                    f"spool-spurctld-{i}", self.namespace
                )
            except ApiException as exc:
                if not _is_not_found(exc):
                    raise

        self._wait_pods_gone("app=spurctld")
        self._wait_pods_gone("app=spur-k8s-operator")
        self._wait_pods_gone("app=spur-postgres", raise_on_timeout=False)
        logger.info("workload teardown complete")

    def cleanup_test_workloads(self) -> None:
        """Remove SpurJobs and their pods between tests; keep controller/operator."""
        try:
            jobs = self.custom_api.list_namespaced_custom_object(
                group=SPUR_JOB_GROUP,
                version=SPUR_JOB_VERSION,
                namespace=self.namespace,
                plural=SPUR_JOB_PLURAL,
            )
        except ApiException:
            jobs = {"items": []}

        for item in jobs.get("items", []):
            name = item.get("metadata", {}).get("name")
            if name:
                self.delete_spurjob(name)

        self._force_delete_pods("spur.amd.com/job-name")
        self._wait_pods_gone(
            "spur.amd.com/job-name", timeout=30, raise_on_timeout=False
        )

    def ensure_controllers_ready(self, timeout: int = 90) -> None:
        expected = self.config.replicas
        wait_until(
            lambda: self._ready_controller_count() >= expected,
            timeout,
            f"expected {expected} ready controller pod(s)",
        )
        wait_until(
            lambda: self._operator_available(),
            timeout,
            "operator deployment not ready",
        )

    def spur_cli(self, args: list[str]) -> str:
        """Run the Spur CLI inside the controller pod.

        The controller pod is the only place that has both the binary and a
        loopback route to spurctld, so admin commands go through it.
        """
        cmd = " ".join(shlex.quote(a) for a in ["spur", *args])
        return exec_in_pod(self.namespace, "spurctld-0", ["sh", "-c", f"{cmd} 2>&1"])

    def operator_pods(self) -> list:
        return self.core_v1.list_namespaced_pod(
            self.namespace, label_selector="app=spur-k8s-operator"
        ).items

    def operator_logs(self, tail_lines: int = 400) -> str:
        return "\n".join(
            self.core_v1.read_namespaced_pod_log(
                pod.metadata.name, self.namespace, tail_lines=tail_lines
            )
            for pod in self.operator_pods()
        )

    def restart_operator(self, timeout: int = 150) -> None:
        """Delete the operator pod and wait for a fresh one to be ready.

        Startup-only paths (orphan pod cleanup) are only reachable this way.
        """
        before = {pod.metadata.name for pod in self.operator_pods()}
        self._force_delete_pods("app=spur-k8s-operator")

        def replaced() -> bool:
            pods = self.operator_pods()
            fresh = [p for p in pods if p.metadata.name not in before]
            return bool(fresh) and all(
                p.status.phase == "Running" for p in fresh
            )

        wait_until(replaced, timeout, "operator pod was not replaced", interval=3)
        wait_until(
            self._operator_available, timeout, "operator deployment not ready", interval=3
        )

    def create_spurjob(self, body: dict) -> dict:
        return self.custom_api.create_namespaced_custom_object(
            group=SPUR_JOB_GROUP,
            version=SPUR_JOB_VERSION,
            namespace=self.namespace,
            plural=SPUR_JOB_PLURAL,
            body=body,
        )

    def get_spurjob(self, name: str, namespace: str | None = None) -> dict:
        ns = namespace or self.namespace
        return self.custom_api.get_namespaced_custom_object(
            group=SPUR_JOB_GROUP,
            version=SPUR_JOB_VERSION,
            namespace=ns,
            plural=SPUR_JOB_PLURAL,
            name=name,
        )

    def delete_spurjob(self, name: str, namespace: str | None = None) -> None:
        ns = namespace or self.namespace
        try:
            self.custom_api.delete_namespaced_custom_object(
                group=SPUR_JOB_GROUP,
                version=SPUR_JOB_VERSION,
                namespace=ns,
                plural=SPUR_JOB_PLURAL,
                name=name,
            )
        except ApiException as exc:
            if not _is_not_found(exc):
                raise


def base_spec(name: str, command: list[str], num_nodes: int = 1) -> dict:
    return {
        "name": name,
        "image": "busybox:latest",
        "gpus": {},
        "numNodes": num_nodes,
        "tasksPerNode": 1,
        "cpusPerTask": 1,
        "memoryPerNode": "100Mi",
        "command": command,
        "args": [],
        "env": {},
        "secretEnv": {},
        "volumes": [],
        "hostNetwork": False,
        "privileged": False,
        "hostIpc": False,
        "extraResources": {},
        "tolerations": [],
        "nodeSelector": {},
        "dependencies": [],
    }


def spurjob_body(name: str, spec: dict) -> dict:
    return {
        "apiVersion": f"{SPUR_JOB_GROUP}/{SPUR_JOB_VERSION}",
        "kind": "SpurJob",
        "metadata": {"name": name},
        "spec": spec,
    }


def simple_spurjob(name: str, command: list[str]) -> dict:
    return spurjob_body(name, base_spec(name, command, 1))


def spurjob_with_env(name: str, command: list[str], env: dict[str, str]) -> dict:
    spec = base_spec(name, command, 1)
    spec["env"] = env
    return spurjob_body(name, spec)


def multinode_spurjob(name: str, command: list[str], num_nodes: int) -> dict:
    return spurjob_body(name, base_spec(name, command, num_nodes))


def spurjob_with_spec(name: str, command: list[str], **overrides) -> dict:
    """SpurJob with arbitrary spec fields layered onto the defaults.

    Keys are CRD field names (camelCase), e.g. ``hostNetwork=True`` or
    ``volumes=["/tmp:/mnt:ro"]``.
    """
    spec = base_spec(name, command, overrides.pop("numNodes", 1))
    spec.update(overrides)
    return spurjob_body(name, spec)


def gpu_spurjob(name: str, command: list[str], count: int, gpu_type: str | None = None) -> dict:
    gpus: dict = {"count": count}
    if gpu_type:
        gpus["gpuType"] = gpu_type
    return spurjob_with_spec(name, command, gpus=gpus)


def volume_spurjob(name: str, command: list[str], volumes: list[str]) -> dict:
    return spurjob_with_spec(name, command, volumes=volumes)


def secret_env_spurjob(name: str, command: list[str], secret_env: dict[str, str]) -> dict:
    return spurjob_with_spec(name, command, secretEnv=secret_env)


def security_spurjob(
    name: str,
    command: list[str],
    *,
    host_network: bool = False,
    host_ipc: bool = False,
    privileged: bool = False,
    shm_size: str | None = None,
) -> dict:
    overrides: dict = {
        "hostNetwork": host_network,
        "hostIpc": host_ipc,
        "privileged": privileged,
    }
    if shm_size:
        overrides["shmSize"] = shm_size
    return spurjob_with_spec(name, command, **overrides)


def resource_spurjob(
    name: str,
    command: list[str],
    *,
    tasks_per_node: int = 1,
    cpus_per_task: int = 1,
    memory_per_node: str = "100Mi",
    extra_resources: dict[str, str] | None = None,
) -> dict:
    return spurjob_with_spec(
        name,
        command,
        tasksPerNode=tasks_per_node,
        cpusPerTask=cpus_per_task,
        memoryPerNode=memory_per_node,
        extraResources=extra_resources or {},
    )


def list_spurjob_pods(fixture: ClusterFixture, name: str, namespace: str | None = None):
    ns = namespace or fixture.namespace
    return fixture.core_v1.list_namespaced_pod(
        ns, label_selector=f"spur.amd.com/job-name={name}"
    ).items


def wait_spurjob_pod(
    fixture: ClusterFixture,
    name: str,
    timeout: int = DEFAULT_TIMEOUT,
    namespace: str | None = None,
):
    """Wait for the operator to create a pod for the SpurJob and return it."""
    pods: list = []

    def created() -> bool:
        nonlocal pods
        pods = list_spurjob_pods(fixture, name, namespace)
        return bool(pods)

    wait_until(created, timeout, f"no pods created for SpurJob {name}")
    return pods[0]


def _resolve_attr(obj, dotted: str):
    for part in dotted.split("."):
        if obj is None:
            return None
        if isinstance(obj, dict):
            obj = obj.get(part)
        elif isinstance(obj, list):
            obj = obj[int(part)]
        else:
            obj = getattr(obj, part)
    return obj


def assert_pod_spec(
    fixture: ClusterFixture,
    name: str,
    expected: dict,
    namespace: str | None = None,
) -> None:
    """Assert dotted paths on the pod created for a SpurJob.

    Paths are relative to the pod and use the Python client's snake_case
    attribute names, e.g. ``spec.host_network`` or
    ``spec.containers.0.resources.limits``.
    """
    pod = wait_spurjob_pod(fixture, name, namespace=namespace)
    for path, want in expected.items():
        got = _resolve_attr(pod, path)
        assert got == want, (
            f"SpurJob {name} pod {pod.metadata.name}: {path} is {got!r}, "
            f"expected {want!r}"
        )


def job_service_exists(
    fixture: ClusterFixture, spur_job_id: int | str, namespace: str | None = None
) -> bool:
    """Whether the headless Service for a multi-node job still exists.

    The operator names it after the Spur job ID (`spur-job-{id}`), not the
    SpurJob resource name.
    """
    ns = namespace or fixture.namespace
    try:
        fixture.core_v1.read_namespaced_service(f"spur-job-{spur_job_id}", ns)
        return True
    except ApiException as exc:
        if _is_not_found(exc):
            return False
        raise


def read_all_spurjob_pod_logs(
    fixture: ClusterFixture,
    name: str,
    namespace: str | None = None,
) -> dict[str, str]:
    """Logs of every pod backing a SpurJob, keyed by pod name."""
    ns = namespace or fixture.namespace
    pods = list_spurjob_pods(fixture, name, ns)
    if not pods:
        raise AssertionError(f"no pods found for SpurJob {name} in namespace {ns}")
    return {
        pod.metadata.name: fixture.core_v1.read_namespaced_pod_log(
            pod.metadata.name, ns
        )
        for pod in pods
    }


def wait_spurjob_state(
    fixture: ClusterFixture,
    name: str,
    target: str,
    timeout: int = DEFAULT_TIMEOUT,
    namespace: str | None = None,
) -> dict:
    ns = namespace or fixture.namespace
    terminal = {"Completed", "Failed", "Cancelled", "Timeout", "NodeFail"}
    result: dict = {}

    def check() -> bool:
        nonlocal result
        job = fixture.get_spurjob(name, namespace=ns)
        state = (job.get("status") or {}).get("state", "")
        if state == target:
            result = job
            return True
        if state in terminal and state != target:
            raise RuntimeError(
                f"SpurJob {name} reached terminal state '{state}', wanted '{target}'"
            )
        return False

    wait_until(check, timeout, f"SpurJob {name} did not reach state '{target}'")
    return result


def read_spurjob_pod_logs(
    fixture: ClusterFixture,
    name: str,
    namespace: str | None = None,
) -> str:
    ns = namespace or fixture.namespace
    pods = fixture.core_v1.list_namespaced_pod(
        ns, label_selector=f"spur.amd.com/job-name={name}"
    )
    if not pods.items:
        raise AssertionError(f"no pods found for SpurJob {name} in namespace {ns}")
    pod_name = pods.items[0].metadata.name
    return fixture.core_v1.read_namespaced_pod_log(pod_name, ns)


def assert_leader_elected(namespace: str, replica_count: int = 3) -> None:
    def check() -> bool:
        for i in range(replica_count):
            vote = read_file_from_pod(
                namespace, f"spurctld-{i}", "/var/spool/spur/raft/vote.json"
            )
            if '"committed":true' in vote.replace(" ", ""):
                return True
        return False

    assert_eventually(HA_TIMEOUT, WAIT_INTERVAL, "no committed leader vote", check)


def wait_spurjob_pods_exist(
    fixture: ClusterFixture,
    name: str,
    timeout: int = DEFAULT_TIMEOUT,
) -> None:
    """Wait until the operator has created at least one pod for the SpurJob."""
    wait_until(
        lambda: bool(
            fixture.core_v1.list_namespaced_pod(
                fixture.namespace,
                label_selector=f"spur.amd.com/job-name={name}",
            ).items
        ),
        timeout,
        f"no pods created for SpurJob {name}",
    )


def ensure_namespace(name: str) -> None:
    _load_kube_config()
    core = client.CoreV1Api()
    body = client.V1Namespace(metadata=client.V1ObjectMeta(name=name))
    try:
        core.create_namespace(body)
    except ApiException as exc:
        if not _is_already_exists(exc):
            raise


def delete_namespace(name: str, *, wait: bool = False, timeout: int = 120) -> None:
    _load_kube_config()
    core = client.CoreV1Api()
    try:
        core.delete_namespace(name)
    except ApiException as exc:
        if not _is_not_found(exc):
            raise

    if not wait:
        return

    def gone() -> bool:
        try:
            core.read_namespace(name)
            return False
        except ApiException as exc:
            return _is_not_found(exc)

    try:
        wait_until(gone, timeout, f"namespace {name} not deleted")
    except TimeoutError:
        logger.warning("timed out waiting for namespace %s to be deleted", name)


def service_http_get(
    namespace: str, service: str, port: str | int, path: str
) -> tuple[int, str]:
    """GET an in-cluster HTTP endpoint through the apiserver's service proxy.

    The Spur image ships no HTTP client, so proxying through the apiserver is
    the only way to reach operator endpoints without adding a sidecar.
    """
    _load_kube_config()
    core = client.CoreV1Api()
    try:
        body = core.connect_get_namespaced_service_proxy_with_path(
            f"{service}:{port}", namespace, path.lstrip("/")
        )
        return 200, body if isinstance(body, str) else str(body)
    except ApiException as exc:
        return exc.status, exc.body or ""


def exec_in_pod(namespace: str, pod_name: str, command: list[str]) -> str:
    _load_kube_config()
    core = client.CoreV1Api()
    resp = stream(
        core.connect_get_namespaced_pod_exec,
        pod_name,
        namespace,
        command=command,
        stderr=False,
        stdin=False,
        stdout=True,
        tty=False,
    )
    return resp


def read_file_from_pod(namespace: str, pod_name: str, path: str) -> str:
    # kubernetes.stream rewrites JSON-looking exec stdout into Python dict syntax,
    # so read the raw bytes via base64 instead of cat.
    quoted = shlex.quote(path)
    encoded = exec_in_pod(
        namespace,
        pod_name,
        ["sh", "-c", f"base64 {quoted} | tr -d '\\n'"],
    )
    return base64.b64decode(encoded.strip()).decode()


def delete_pod(namespace: str, name: str) -> None:
    _load_kube_config()
    core = client.CoreV1Api()
    core.delete_namespaced_pod(
        name,
        namespace,
        grace_period_seconds=0,
        propagation_policy="Background",
    )


def wait_pod_ready(namespace: str, pod_name: str, timeout: int = 120) -> None:
    _load_kube_config()
    core = client.CoreV1Api()

    def ready() -> bool:
        try:
            pod = core.read_namespaced_pod(pod_name, namespace)
        except ApiException as exc:
            if _is_not_found(exc):
                return False
            raise
        return any(
            c.type == "Ready" and c.status == "True"
            for c in (pod.status.conditions or [])
        )

    wait_until(ready, timeout, f"pod {pod_name} not ready")


def count_ready_pods(namespace: str, label_selector: str) -> int:
    _load_kube_config()
    core = client.CoreV1Api()
    pods = core.list_namespaced_pod(namespace, label_selector=label_selector)
    return sum(
        1
        for pod in pods.items
        if any(
            c.type == "Ready" and c.status == "True"
            for c in (pod.status.conditions or [])
        )
    )


def assert_eventually(
    timeout: int,
    interval: int,
    msg: str,
    check_fn: Callable[[], bool],
) -> None:
    try:
        wait_until(check_fn, timeout, msg, interval=interval)
    except TimeoutError as exc:
        raise AssertionError(str(exc)) from exc
