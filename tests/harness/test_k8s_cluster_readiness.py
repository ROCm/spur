# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for the K8s fixture's readiness gates and image pre-pull.

A fake CoreV1 client replays scripted pod phases and a fake clock stands in for
`time`, so the real helpers run without a cluster or sleeping.
"""

from types import SimpleNamespace as NS

import pytest

import k8s_cluster
from k8s_cluster import (
    JOB_IMAGE,
    MANAGED_NODE_LABEL,
    PREPULL_SELECTOR,
    ClusterFixture,
    FixtureConfig,
    base_spec,
    managed_node_names,
    prepull_image,
    prepull_pod,
)


class FakeClock:
    def __init__(self):
        self.now = 1000.0

    def time(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


@pytest.fixture(autouse=True)
def clock(monkeypatch):
    fake = FakeClock()
    monkeypatch.setattr(k8s_cluster, "time", fake)
    return fake


def status(phase, waiting=None):
    state = NS(waiting=NS(reason=waiting, message=None)) if waiting else None
    return NS(phase=phase, container_statuses=[NS(state=state)] if state else [])


class FakeCore:
    """`outcomes` maps a node to the phase its prepull pod reaches, or None to stay Pending."""

    def __init__(self, nodes, outcomes=None):
        self.nodes = nodes
        self.outcomes = outcomes or {}
        self.created = []
        self.deleted = []
        self.selectors = []

    def list_node(self, label_selector=None):
        self.selectors.append(label_selector)
        return NS(items=[NS(metadata=NS(name=n)) for n in self.nodes])

    def create_namespaced_pod(self, namespace, body):
        self.created.append(body)

    def delete_collection_namespaced_pod(self, namespace, **kwargs):
        self.deleted.append(kwargs)
        self.created = []

    def list_namespaced_pod(self, namespace, label_selector=None):
        assert label_selector == PREPULL_SELECTOR
        items = []
        for pod in self.created:
            node = pod.spec.node_name
            phase = self.outcomes.get(node, "Succeeded")
            waiting = "ImagePullBackOff" if phase is None else None
            items.append(NS(
                metadata=pod.metadata,
                spec=pod.spec,
                status=status(phase or "Pending", waiting),
            ))
        return NS(items=items)


def test_managed_nodes_are_queried_by_the_managed_label_and_sorted():
    core = FakeCore(["w3", "w1", "w2"])
    assert managed_node_names(core) == ["w1", "w2", "w3"]
    assert core.selectors == [MANAGED_NODE_LABEL]


def test_prepull_pod_is_pinned_to_the_node_and_never_forces_a_pull():
    pod = prepull_pod("prepull-0", "w1", JOB_IMAGE)
    container = pod.spec.containers[0]
    assert pod.spec.node_name == "w1"
    assert pod.spec.restart_policy == "Never"
    assert container.image == JOB_IMAGE
    assert container.image_pull_policy == "IfNotPresent"
    assert container.command == ["true"]
    assert pod.metadata.labels == {"app": "spur-e2e-prepull"}


def test_job_image_is_the_one_every_spurjob_uses():
    assert base_spec("j", ["true"])["image"] == JOB_IMAGE


def test_prepull_creates_one_pod_per_managed_worker_and_cleans_up():
    core = FakeCore(["w1", "w2", "w3"])
    created_nodes = []
    original = core.create_namespaced_pod

    def spy(namespace, body):
        created_nodes.append(body.spec.node_name)
        original(namespace, body)

    core.create_namespaced_pod = spy
    prepull_image(core, "ns", JOB_IMAGE, timeout=30)
    assert created_nodes == ["w1", "w2", "w3"]
    assert core.deleted[-1]["label_selector"] == PREPULL_SELECTOR


def test_prepull_clears_leftovers_before_creating_pods():
    core = FakeCore(["w1"])
    order = []
    core.delete_collection_namespaced_pod = lambda ns, **kw: order.append("delete")
    core.create_namespaced_pod = lambda ns, body: order.append("create")
    core.list_namespaced_pod = lambda ns, label_selector=None: NS(items=[NS(
        metadata=NS(name="p"), spec=NS(node_name="w1"), status=status("Succeeded"))])
    prepull_image(core, "ns", JOB_IMAGE, timeout=30)
    assert order == ["delete", "create", "delete"]


def test_prepull_timeout_names_the_stuck_worker_and_still_deletes_the_pods():
    core = FakeCore(["w1", "w2"], outcomes={"w2": None})
    with pytest.raises(TimeoutError) as err:
        prepull_image(core, "ns", JOB_IMAGE, timeout=10)
    message = str(err.value)
    assert "timed out after 10s" in message
    assert "prepull-1 on w2: ImagePullBackOff" in message
    assert "prepull-0" not in message
    assert core.deleted[-1]["label_selector"] == PREPULL_SELECTOR
    assert core.created == []


def test_prepull_failed_pod_fails_fast_and_cleans_up(clock):
    core = FakeCore(["w1"], outcomes={"w1": "Failed"})
    with pytest.raises(RuntimeError, match=r"pre-pull of .* failed: prepull-0 on w1: Failed"):
        prepull_image(core, "ns", JOB_IMAGE, timeout=300)
    assert clock.now == 1000.0
    assert core.created == []


def test_prepull_without_managed_nodes_is_an_error_not_a_silent_pass():
    core = FakeCore([])
    with pytest.raises(RuntimeError, match=MANAGED_NODE_LABEL):
        prepull_image(core, "ns", JOB_IMAGE)
    assert core.created == []


class GateFixture(ClusterFixture):
    """A ClusterFixture whose probes are scripted, so wait_ready's gates can be driven."""

    def __init__(self, *, replicas=3, controllers=3, operator=True, registered=3, workers=("w1", "w2", "w3")):
        self.config = FixtureConfig(replicas=replicas, image="img", config_toml="")
        self.core_v1 = FakeCore(list(workers))
        self.controllers = controllers
        self.operator = operator
        self.registered = registered

    def _ready_controller_count(self):
        return self.controllers

    def _operator_available(self):
        return self.operator

    def _registered_node_count(self):
        return self.registered


def test_wait_ready_passes_when_every_gate_is_met():
    GateFixture().wait_ready()


def test_each_gate_timeout_names_its_gate_and_uses_the_full_budget(clock):
    start = clock.now
    with pytest.raises(TimeoutError, match=r"timed out after 300s: gate 'controllers ready': expected 3"):
        GateFixture(controllers=2).wait_ready()
    assert clock.now - start >= 300

    with pytest.raises(TimeoutError, match=r"timed out after 300s: gate 'operator ready'"):
        GateFixture(operator=False).wait_ready()


def test_a_slow_early_gate_does_not_eat_the_next_gates_budget(clock):
    fixture = GateFixture(registered=0)
    ready_at = clock.now + 250

    fixture._operator_available = lambda: clock.now >= ready_at
    start = clock.now
    with pytest.raises(TimeoutError, match=r"gate 'workers registered'"):
        fixture.wait_ready()
    assert clock.now - start >= 250 + 300


def test_wait_ready_requires_every_managed_worker_not_just_one():
    with pytest.raises(TimeoutError, match=r"gate 'workers registered'.*all 3 managed worker"):
        GateFixture(registered=2).wait_ready()


def test_expected_worker_count_follows_the_cluster_not_a_constant():
    GateFixture(registered=1, workers=("only",)).wait_ready()
    with pytest.raises(TimeoutError):
        GateFixture(registered=3, workers=("a", "b", "c", "d")).wait_ready()


def test_wait_ready_fails_clearly_when_no_worker_is_managed():
    with pytest.raises(RuntimeError, match=MANAGED_NODE_LABEL):
        GateFixture(workers=()).wait_ready()
