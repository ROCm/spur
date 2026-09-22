# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Multi-control-plane (HA) native k0s E2E tests (needs >= 3 nodes for etcd quorum).

Coverage:
- ``test_three_control_planes_form_etcd_quorum``: quorum formation.
- ``test_even_replica_count_rejected``: input validation.
- ``test_rolling_upgrade_preserves_etcd_quorum``: rolling controller + agent
  restarts preserve etcd quorum, canary workloads, and k8s metrics.
"""

from __future__ import annotations

import re
import time

import pytest

from cluster import SpurCluster

BUSYBOX_IMAGE = "docker.io/library/busybox:1.36"

# --- helpers (self-contained; mirrors test_upgrade_k0s.py) ------------------


def _k0s_bin(c: SpurCluster) -> str:
    return "$(command -v k0s || echo /usr/local/bin/k0s)"


def _kubectl(c: SpurCluster, args: str) -> str:
    cp_nodes = c.k8s_control_planes()
    cp_idx = c.node_names.index(cp_nodes[0]) if cp_nodes else 0
    return c.nodes[cp_idx].exec_allow_fail(
        f"{c._sudo_prefix()}{_k0s_bin(c)} kubectl {args}"
    )


def _k8s_nodes_ready(c: SpurCluster, names: list[str]) -> bool:
    out = _kubectl(c, "get nodes --no-headers")
    ready = set()
    for line in out.splitlines():
        f = line.split()
        if len(f) >= 2 and f[1] == "Ready":
            ready.add(f[0])
    for name in names:
        if not any(r == name or r.startswith(name + ".") for r in ready):
            return False
    return True


def _wait_k8s_nodes_ready(c: SpurCluster, names: list[str],
                          timeout: int = 300) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _k8s_nodes_ready(c, names):
            return
        time.sleep(5)
    out = _kubectl(c, "get nodes -o wide 2>&1")
    raise TimeoutError(f"k8s nodes not all Ready within {timeout}s:\n{out}")


def _deploy_canary(c: SpurCluster, name: str, replicas: int) -> None:
    manifest = (
        "apiVersion: apps/v1\n"
        "kind: Deployment\n"
        f"metadata:\n  name: {name}\n"
        "spec:\n"
        f"  replicas: {replicas}\n"
        "  selector:\n    matchLabels:\n"
        f"      app: {name}\n"
        "  template:\n"
        "    metadata:\n"
        f"      labels:\n        app: {name}\n"
        "    spec:\n"
        "      containers:\n"
        f"      - name: {name}\n"
        f"        image: {BUSYBOX_IMAGE}\n"
        "        imagePullPolicy: IfNotPresent\n"
        "        command: ['sh','-c','sleep 3600']\n"
        "      restartPolicy: Always\n"
        "      terminationGracePeriodSeconds: 0\n"
    )
    cp_nodes = c.k8s_control_planes()
    cp_idx = c.node_names.index(cp_nodes[0]) if cp_nodes else 0
    remote_path = f"/tmp/{name}-deploy.yaml"
    c.nodes[cp_idx].write_file(remote_path, manifest)
    out = c.nodes[cp_idx].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    assert "created" in out or "configured" in out or "unchanged" in out, (
        f"kubectl apply for deployment {name} did not succeed:\n{out}"
    )


def _wait_deployment_ready(c: SpurCluster, name: str,
                           timeout: int = 300) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        out = _kubectl(
            c,
            f"get deployment {name} -o jsonpath="
            f"'{{.status.readyReplicas}}'",
        )
        desired = _kubectl(
            c,
            f"get deployment {name} -o jsonpath="
            f"'{{.spec.replicas}}'",
        )
        if out.strip().isdigit() and desired.strip().isdigit():
            if int(out.strip()) >= int(desired.strip()):
                return
        time.sleep(5)
    status = _kubectl(c, f"get deployment {name} -o wide")
    raise TimeoutError(
        f"deployment {name} not ready within {timeout}s:\n{status}"
    )


def _assert_no_pod_restarts(c: SpurCluster, label: str) -> None:
    out = _kubectl(
        c,
        f"get pods -l {label} "
        f"-o jsonpath='{{range .items[*]}}{{range .status.containerStatuses[*]}}"
        f"{{.restartCount}}{{\"\\n\"}}{{end}}{{end}}'",
    )
    for line in out.strip().splitlines():
        line = line.strip()
        if line and line.isdigit():
            assert int(line) == 0, (
                f"pod restart detected (restartCount={line}) for {label}"
            )


def _assert_canary_survived(c: SpurCluster, name: str) -> None:
    _wait_deployment_ready(c, name, timeout=120)
    _assert_no_pod_restarts(c, f"app={name}")


def _prepull_busybox(c: SpurCluster) -> bool:
    sudo = c._sudo_prefix()
    k0s = "$(command -v k0s || echo /usr/local/bin/k0s)"
    for node in c.nodes:
        sock_check = node.exec_allow_fail(
            "test -S /run/k0s/containerd.sock && echo yes || echo no"
        ).strip()
        if sock_check != "yes":
            continue
        out = node.exec_allow_fail(
            f"{sudo}{k0s} ctr -n k8s.io images pull {BUSYBOX_IMAGE} 2>&1"
        )
        if "done" in out or "already exists" in out:
            continue
        node.exec_allow_fail(
            f"{sudo}{k0s} ctr -n k8s.io images import"
            f" /tmp/busybox-1.36.tar 2>/dev/null || true"
        )
        present = node.exec_allow_fail(
            f"{sudo}{k0s} ctr -n k8s.io images ls -q 2>/dev/null"
            f" | grep -c busybox"
        ).strip()
        if present == "0":
            return False
    return True


def _delete_k8s_resource(c: SpurCluster, kind: str, name: str) -> None:
    _kubectl(c, f"delete {kind} {name} --ignore-not-found 2>/dev/null || true")


def _fetch_metrics_k8s(c: SpurCluster) -> str:
    return c.nodes[0].exec_allow_fail(
        "curl -sf http://127.0.0.1:6822/metrics/k8s 2>/dev/null || true"
    )


def _assert_k8s_metrics_phase(c: SpurCluster, expected: str) -> None:
    body = _fetch_metrics_k8s(c)
    pattern = re.compile(
        rf'spur_k8s_cluster_phase{{[^}}]*phase="{expected}"[^}}]*}}\s+1'
    )
    assert pattern.search(body), (
        f"spur_k8s_cluster_phase for phase={expected} not 1 in:\n{body}"
    )


def _assert_k8s_metrics_cluster_up(c: SpurCluster) -> None:
    body = _fetch_metrics_k8s(c)
    pattern = re.compile(r'spur_k8s_cluster_up\{[^}]*\}\s+1')
    assert pattern.search(body), (
        f"spur_k8s_cluster_up not 1 in:\n{body}"
    )


def _drain_and_restart_agent(c: SpurCluster, node_index: int) -> None:
    node_name = c.node_names[node_index]
    c.cli(["spur", "node", "drain", node_name, "--reason", "rolling-upgrade"])

    deadline = time.time() + 30
    while time.time() < deadline:
        nodes = c.sinfo_nodes()
        if node_name in nodes and "drain" in nodes[node_name]:
            break
        time.sleep(2)

    c.restart_agent(node_index=node_index)
    deadline = time.time() + 60
    while time.time() < deadline:
        if node_name in c.sinfo_nodes():
            return
        time.sleep(2)
    raise AssertionError(
        f"{node_name} did not re-register within 60s after agent restart"
    )


# --- tests ------------------------------------------------------------------


@pytest.mark.k0s
class TestK8sMultiControlPlane:
    def test_three_control_planes_form_etcd_quorum(self, k8s_multicp_cluster):
        cluster = k8s_multicp_cluster

        out = cluster.k8s_up(["--replicas", "3"])
        assert "provisioning requested" in out, out

        cluster.wait_k8s_phase("ready", timeout=600)

        # Three distinct nodes were assigned the control-plane role.
        cps = cluster.k8s_control_planes()
        assert len(cps) == 3, f"expected 3 control planes, got {cps}"
        assert len(set(cps)) == 3, f"control planes not distinct: {cps}"

        # All three report an active controller component.
        active = cluster.k8s_active_controllers()
        assert len(active) == 3, f"expected 3 active controllers, got {active}"

        # Ground truth: the embedded etcd formed a real 3-member quorum. etcd
        # membership converges shortly after the cluster reports ready.
        members = cluster.wait_etcd_members(3)
        assert members == 3, (
            f"expected a 3-member etcd quorum, got {members}\n{cluster.k8s_status()}"
        )

    def test_even_replica_count_rejected(self, k8s_multicp_cluster):
        cluster = k8s_multicp_cluster
        # k0s up is admin-gated; run as root to reach the replica-count validation.
        out = cluster.cli_as_user("root", ["spur", "k8s", "up", "--replicas", "2"])
        assert "1, 3, or 5" in out, f"even replica count must be rejected: {out}"

    def test_rolling_upgrade_preserves_etcd_quorum(self, k8s_multicp_cluster):
        """Rolling controller + agent restarts must not break a 3-CP etcd
        quorum. Mirrors rolling_upgrade.yml: restart controller, then rolling
        drain+restart each agent — etcd quorum, canary workloads, and metrics
        survive every step."""
        c = k8s_multicp_cluster

        # --- bring up 3-CP k0s ---
        out = c.k8s_up(["--replicas", "3"])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        cps = c.k8s_control_planes()
        assert len(cps) == 3, f"expected 3 CPs, got {cps}"
        members_before = c.wait_etcd_members(3)
        assert members_before == 3, (
            f"etcd quorum not 3 before upgrade: {members_before}"
        )

        # Pure controllers don't run kubelet, so `kubectl get nodes` is
        # empty when every node is a CP.  Data-plane (canary, pod scheduling)
        # is validated by test_upgrade_k0s; this test focuses on etcd quorum
        # and control-plane stability through rolling restarts.
        cp_set = set(cps)
        worker_names = [n for n in c.node_names if n not in cp_set]
        has_workers = bool(worker_names)
        if has_workers:
            _wait_k8s_nodes_ready(c, worker_names, timeout=300)

        if has_workers:
            if not _prepull_busybox(c):
                pytest.skip("cannot pre-pull busybox image (offline registry)")
            _deploy_canary(c, "canary-multicp", len(worker_names))
            _wait_deployment_ready(c, "canary-multicp", timeout=300)

        cp_list_before = list(cps)

        # --- step 1: controller restart ---
        c.restart_controller()
        c.wait_k8s_phase("ready", timeout=120)

        assert c.k8s_control_planes() == cp_list_before, (
            f"CP list changed after controller restart: "
            f"{c.k8s_control_planes()} != {cp_list_before}"
        )
        etcd_after_ctrl = c.wait_etcd_members(3, timeout=60)
        assert etcd_after_ctrl == 3, (
            f"etcd quorum lost after controller restart: {etcd_after_ctrl}"
        )
        active_after_ctrl = c.k8s_active_controllers()
        assert len(active_after_ctrl) == 3, (
            f"active controllers != 3 after restart: {active_after_ctrl}"
        )
        if has_workers:
            _assert_canary_survived(c, "canary-multicp")
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)

        # --- step 2: rolling drain+restart of every agent ---
        for i in range(len(c.nodes)):
            _drain_and_restart_agent(c, i)

        c.wait_k8s_phase("ready", timeout=120)
        if has_workers:
            _wait_k8s_nodes_ready(c, worker_names, timeout=120)

        assert c.k8s_control_planes() == cp_list_before, (
            f"CP list changed after rolling agent restart: "
            f"{c.k8s_control_planes()} != {cp_list_before}"
        )
        etcd_after_agents = c.wait_etcd_members(3, timeout=60)
        assert etcd_after_agents == 3, (
            f"etcd quorum lost after rolling agent restart: {etcd_after_agents}"
        )
        assert len(c.k8s_active_controllers()) == 3, (
            f"active controllers != 3 after agent restarts: "
            f"{c.k8s_active_controllers()}"
        )
        if has_workers:
            _assert_canary_survived(c, "canary-multicp")
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)

        # --- cleanup ---
        if has_workers:
            _delete_k8s_resource(c, "deployment", "canary-multicp")
