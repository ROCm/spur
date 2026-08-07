# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""K8s node watcher: node lifecycle projected into Spur node state.

The operator watches Nodes carrying `spur.amd.com/managed=true`, registers them
with spurctld from their allocatable resources, and mirrors health transitions.
Tests that mutate a real Node (taints, deletion) are destructive to whatever
else the cluster is running, so they are opt-in.
"""

import os
import re

import pytest
from kubernetes import client
from kubernetes.client.exceptions import ApiException

from k8s_cluster import assert_eventually, simple_spurjob, wait_spurjob_state

MANAGED_SELECTOR = "spur.amd.com/managed=true"
NOT_READY_TAINT = "node.kubernetes.io/not-ready"

DESTRUCTIVE = os.environ.get("SPUR_TEST_DESTRUCTIVE_NODES") == "1"
destructive = pytest.mark.skipif(
    not DESTRUCTIVE,
    reason="tainting or deleting a real Node disrupts the cluster; "
    "set SPUR_TEST_DESTRUCTIVE_NODES=1 to run",
)


def managed_nodes() -> list:
    return client.CoreV1Api().list_node(label_selector=MANAGED_SELECTOR).items


UP_STATES = {"idle", "mix", "alloc"}


def spur_nodes(cluster) -> dict[str, str]:
    """Node name to state, as spurctld sees it."""
    out = cluster.spur_cli(["sinfo", "-N", "-h", "-o", "%N %T"])
    states = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            states[parts[0]] = parts[1].lower()
    return states


def scontrol_field(cluster, node: str, field: str) -> str | None:
    out = cluster.spur_cli(["scontrol", "show", "node", node])
    match = re.search(rf"{field}=(\S+)", out)
    return match.group(1) if match else None


def pod_host_nodes(cluster) -> set[str]:
    """Nodes hosting the control plane; those must not be tainted by a test."""
    pods = cluster.core_v1.list_namespaced_pod(cluster.namespace)
    return {p.spec.node_name for p in pods.items if p.spec.node_name}


@pytest.fixture(scope="class")
def watched_cluster(cluster):
    if not managed_nodes():
        pytest.skip(f"no K8s nodes carry {MANAGED_SELECTOR}")
    return cluster


class TestRegistration:
    def test_every_managed_node_is_registered_with_spurctld(self, watched_cluster):
        expected = {n.metadata.name for n in managed_nodes()}

        def all_present() -> bool:
            return expected <= set(spur_nodes(watched_cluster))

        assert_eventually(
            120,
            5,
            f"spurctld never saw every managed node ({sorted(expected)})",
            all_present,
        )

    def test_registered_cpus_match_the_node_allocatable(self, watched_cluster):
        """Spur schedules against these numbers, so a parsing slip here
        overcommits or strands the whole node."""
        node = managed_nodes()[0]
        allocatable = node.status.allocatable["cpu"]
        expected = (
            int(allocatable[:-1]) // 1000 if allocatable.endswith("m") else int(allocatable)
        )
        reported = scontrol_field(watched_cluster, node.metadata.name, "CPUTot")
        assert reported is not None, "scontrol reported no CPUTot"
        assert int(reported) == expected

    def test_a_managed_node_can_run_a_job(self, watched_cluster):
        """Registration is only meaningful if the node is actually
        schedulable afterwards."""
        watched_cluster.create_spurjob(
            simple_spurjob("it-node-sched", ["sh", "-c", "echo NODE_OK"])
        )
        job = wait_spurjob_state(watched_cluster, "it-node-sched", "Completed")
        assigned = (job.get("status") or {}).get("assignedNodes") or []
        assert assigned, "completed job reported no assigned node"
        assert set(assigned) <= {n.metadata.name for n in managed_nodes()}

    def test_gpu_nodes_advertise_a_vendor_gpu_type(self, watched_cluster):
        """GPU type drives constraint matching, so an unlabelled GPU node must
        still get a vendor-derived default rather than an empty string."""
        gpu_nodes = [
            n
            for n in managed_nodes()
            if (n.status.allocatable or {}).get("amd.com/gpu")
            or (n.status.allocatable or {}).get("nvidia.com/gpu")
        ]
        if not gpu_nodes:
            pytest.skip("no GPU nodes in this cluster")

        node = gpu_nodes[0]
        gres = scontrol_field(watched_cluster, node.metadata.name, "Gres")
        assert gres and gres != "(null)", f"GPU node {node.metadata.name} has no Gres"
        expected_type = (node.metadata.labels or {}).get("spur.amd.com/gpu-type")
        if expected_type is None:
            expected_type = (
                "amd-gpu"
                if (node.status.allocatable or {}).get("amd.com/gpu")
                else "nvidia-gpu"
            )
        assert expected_type in gres, gres

    def test_an_operator_restart_re_registers_every_node(self, watched_cluster):
        """The fingerprint cache lives in memory, so a restart must re-drive
        registration rather than assume spurctld still has the nodes."""
        expected = {n.metadata.name for n in managed_nodes()}
        watched_cluster.restart_operator()

        assert_eventually(
            150,
            5,
            "nodes were not re-registered after an operator restart",
            lambda: expected <= set(spur_nodes(watched_cluster)),
        )
        logs = watched_cluster.operator_logs()
        assert "registering K8s node" in logs, logs[-2000:]


@destructive
class TestHealthTransitions:
    @pytest.fixture
    def spare_node(self, watched_cluster):
        """A managed node that hosts none of the Spur control plane."""
        busy = pod_host_nodes(watched_cluster)
        spare = [n for n in managed_nodes() if n.metadata.name not in busy]
        if not spare:
            pytest.skip("every managed node hosts control-plane pods")
        name = spare[0].metadata.name
        yield name
        _remove_not_ready_taint(name)

    def test_a_not_ready_taint_marks_the_node_down(self, watched_cluster, spare_node):
        _add_not_ready_taint(spare_node)
        assert_eventually(
            120,
            5,
            f"{spare_node} never went down after the NotReady taint",
            lambda: spur_nodes(watched_cluster).get(spare_node) == "down",
        )

    def test_the_down_reason_names_the_taint(self, watched_cluster, spare_node):
        """The reason string is what an operator sees in sinfo, and it is the
        only thing distinguishing a k8s-driven drain from an admin one."""
        _add_not_ready_taint(spare_node)
        assert_eventually(
            120,
            5,
            "spurctld never recorded the NotReady reason",
            lambda: "NotReady"
            in watched_cluster.spur_cli(["scontrol", "show", "node", spare_node]),
        )

    def test_removing_the_taint_resumes_the_node(self, watched_cluster, spare_node):
        _add_not_ready_taint(spare_node)
        assert_eventually(
            120,
            5,
            "node never went down",
            lambda: spur_nodes(watched_cluster).get(spare_node) == "down",
        )

        _remove_not_ready_taint(spare_node)
        assert_eventually(
            120,
            5,
            f"{spare_node} never came back up after the taint was removed",
            lambda: spur_nodes(watched_cluster).get(spare_node) in UP_STATES,
        )

    def test_a_down_node_stops_receiving_work(self, watched_cluster, spare_node):
        """A node marked down that still gets dispatched to would strand jobs
        on an unreachable kubelet."""
        _add_not_ready_taint(spare_node)
        assert_eventually(
            120,
            5,
            "node never went down",
            lambda: spur_nodes(watched_cluster).get(spare_node) == "down",
        )

        watched_cluster.create_spurjob(
            simple_spurjob("it-avoid-down", ["sh", "-c", "echo AVOID_OK"])
        )
        job = wait_spurjob_state(watched_cluster, "it-avoid-down", "Completed")
        assigned = (job.get("status") or {}).get("assignedNodes") or []
        assert spare_node not in assigned, f"job landed on the down node: {assigned}"


@destructive
class TestNodeRemoval:
    def test_deleting_the_node_object_marks_it_down(self, watched_cluster):
        """The kubelet recreates the Node within seconds but without its
        labels, so the test relabels it to put the cluster back."""
        busy = pod_host_nodes(watched_cluster)
        spare = [n for n in managed_nodes() if n.metadata.name not in busy]
        if not spare:
            pytest.skip("every managed node hosts control-plane pods")
        name = spare[0].metadata.name
        labels = dict(spare[0].metadata.labels or {})

        core = client.CoreV1Api()
        core.delete_node(name)
        try:
            assert_eventually(
                120,
                5,
                f"{name} was not marked down after its Node object was deleted",
                lambda: spur_nodes(watched_cluster).get(name) == "down",
            )
            assert "K8s node removed" in watched_cluster.spur_cli(
                ["scontrol", "show", "node", name]
            )
        finally:
            assert_eventually(
                180, 5, f"kubelet never recreated Node {name}", lambda: _node_exists(name)
            )
            core.patch_node(name, {"metadata": {"labels": labels}})

        assert_eventually(
            180,
            5,
            f"{name} never re-registered after being relabelled",
            lambda: spur_nodes(watched_cluster).get(name) in UP_STATES,
        )


def _node_exists(name: str) -> bool:
    try:
        client.CoreV1Api().read_node(name)
        return True
    except ApiException as exc:
        if exc.status == 404:
            return False
        raise


def _add_not_ready_taint(name: str) -> None:
    core = client.CoreV1Api()
    node = core.read_node(name)
    taints = [t.to_dict() for t in (node.spec.taints or [])]
    if any(t["key"] == NOT_READY_TAINT for t in taints):
        return
    taints.append({"key": NOT_READY_TAINT, "effect": "NoSchedule"})
    core.patch_node(name, {"spec": {"taints": taints}})


def _remove_not_ready_taint(name: str) -> None:
    core = client.CoreV1Api()
    node = core.read_node(name)
    taints = [
        t.to_dict() for t in (node.spec.taints or []) if t.key != NOT_READY_TAINT
    ]
    core.patch_node(name, {"spec": {"taints": taints}})
