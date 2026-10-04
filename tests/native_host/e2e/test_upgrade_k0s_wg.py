# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests proving a rolling Spur upgrade does not disrupt a managed
k0s cluster running **over WireGuard** (calico CNI, ``wg_enabled=true``).

Companion to ``test_upgrade_k0s.py`` which covers the no-WireGuard (kuberouter)
topology. This file exercises the production topology where all k8s traffic
(kubelet, etcd, calico BGP, pod-to-pod) rides the ``spur0`` WireGuard mesh.

Coverage matrix position:
- Native Spur (no WG, no k0s) — ``test_deregistration.py``
- Native + managed k0s (no WG) — ``test_upgrade_k0s.py``
- **Spur WG + managed k0s on WG — this file**
- Spur WG only (no k0s) — ``test_wg_mesh.py``

The tests use the ``wg_k0s_cluster`` fixture (conftest.py) which stands up a
rootful Spur cluster with ``wg_enabled=true``, a calico k0s control plane, and a
full WireGuard mesh across all nodes. k0s is NOT yet ``up`` when the fixture
yields — the test controls timing.

Binary-swap upgrade (``TestWgVersionUpgradePreservesK0s``):
  Set ``SPUR_TEST_BASELINE_BINARIES_DIR`` to a directory containing v0.14.0
  (or any prior release) binaries. The cluster starts on those binaries, then
  the test swaps in the PR binaries (``SPUR_TEST_BINARIES_DIR``) and does a
  rolling restart — proving a real version-to-version upgrade preserves k0s.
  Skipped when the env var is unset (CI sets it; local runs can too).
"""

from __future__ import annotations

import logging
import os
import re
import time
from pathlib import Path

import pytest

from cluster import BINARIES
from wg_cluster import WG_IFACE, WgMesh, wait_until

logger = logging.getLogger(__name__)

BUSYBOX_IMAGE = "mirror.gcr.io/library/busybox:1.36"


# --- binary swap helpers ---------------------------------------------------


def _get_baseline_binaries_dir() -> str | None:
    """Return the path to baseline (old-version) binaries, or None if unset."""
    raw = os.environ.get("SPUR_TEST_BASELINE_BINARIES_DIR", "").strip()
    return raw if raw else None


def _get_upgrade_binaries_dir() -> str:
    """The PR/current binaries — same as SPUR_TEST_BINARIES_DIR."""
    repo_root = Path(__file__).resolve().parents[3]
    return os.environ.get(
        "SPUR_TEST_BINARIES_DIR",
        str(repo_root / "target" / "release"),
    )


def _swap_binaries(c, binaries_dir: str) -> None:
    """Replace binaries in c.bin_dir on all nodes with those from binaries_dir.

    Uses upload-to-temp + mv to avoid ETXTBSY when a daemon is still running
    on the old binary. After this, the next restart picks up the new version."""
    for name in BINARIES:
        local_path = Path(binaries_dir) / name
        if not local_path.is_file():
            raise FileNotFoundError(f"Missing binary for swap: {local_path}")
        remote_path = f"{c.bin_dir}/{name}"
        tmp_path = f"{remote_path}.new"
        for node in c.nodes:
            node.upload(str(local_path), tmp_path)
            node.exec(f"chmod +x '{tmp_path}' && mv -f '{tmp_path}' '{remote_path}'")
    logger.info("Swapped binaries on all nodes from %s", binaries_dir)


# --- helpers ----------------------------------------------------------------


def _kubectl(c, args: str) -> str:
    cp_nodes = c.k8s_control_planes()
    cp_idx = c.node_names.index(cp_nodes[0]) if cp_nodes else 0
    return c.nodes[cp_idx].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl {args}"
    )


def _k8s_nodes_ready(c, names: list[str]) -> bool:
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


def _calico_ready(c, cp_index: int) -> bool:
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl "
        "get pods -n kube-system -l k8s-app=calico-node --no-headers 2>/dev/null"
    )
    lines = [ln for ln in out.splitlines() if ln.strip()]
    if not lines:
        return False
    for ln in lines:
        f = ln.split()
        if len(f) < 3 or f[1] != "1/1" or f[2] != "Running":
            return False
    return True


def _deploy_canary(c, name: str, replicas: int) -> None:
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


def _wait_deployment_ready(c, name: str, timeout: int = 300) -> None:
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


def _assert_no_pod_restarts(c, label: str) -> None:
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


def _snapshot_pod_uids(c, label: str) -> set[str]:
    out = _kubectl(
        c,
        f"get pods -l {label} "
        f"-o jsonpath='{{range .items[*]}}{{.metadata.uid}}{{\"\\n\"}}{{end}}'",
    )
    return {u.strip() for u in out.strip().splitlines() if u.strip()}


def _assert_canary_survived(c, name: str,
                            expected_uids: set[str] | None = None) -> None:
    _wait_deployment_ready(c, name, timeout=120)
    _assert_no_pod_restarts(c, f"app={name}")
    if expected_uids:
        actual = _snapshot_pod_uids(c, f"app={name}")
        assert actual == expected_uids, (
            f"canary pods were replaced (evict+recreate): "
            f"expected UIDs {expected_uids}, got {actual}"
        )


def _schedule_new_pod(c, name: str, timeout: int = 180) -> None:
    manifest = (
        "apiVersion: v1\n"
        "kind: Pod\n"
        f"metadata:\n  name: {name}\n"
        "spec:\n"
        "  containers:\n"
        f"  - name: {name}\n"
        f"    image: {BUSYBOX_IMAGE}\n"
        "    imagePullPolicy: IfNotPresent\n"
        "    command: ['sh','-c','echo POST_UPGRADE_OK && sleep 30']\n"
        "  restartPolicy: Never\n"
    )
    cp_nodes = c.k8s_control_planes()
    cp_idx = c.node_names.index(cp_nodes[0]) if cp_nodes else 0
    remote_path = f"/tmp/{name}.yaml"
    c.nodes[cp_idx].write_file(remote_path, manifest)
    out = c.nodes[cp_idx].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    assert "created" in out or "configured" in out or "unchanged" in out, (
        f"kubectl apply for pod {name} did not succeed:\n{out}"
    )

    deadline = time.time() + timeout
    while time.time() < deadline:
        phase = _kubectl(
            c, f"get pod {name} -o jsonpath='{{.status.phase}}'"
        ).strip()
        if phase in ("Running", "Succeeded"):
            return
        time.sleep(5)
    status = _kubectl(c, f"describe pod {name}")
    raise TimeoutError(f"pod {name} not Running within {timeout}s:\n{status}")


def _delete_k8s_resource(c, kind: str, name: str) -> None:
    _kubectl(c, f"delete {kind} {name} --ignore-not-found 2>/dev/null || true")


def _prepull_busybox(c) -> bool:
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


def _fetch_metrics_k8s(c) -> str:
    return c.nodes[0].exec_allow_fail(
        "curl -sf http://127.0.0.1:6822/metrics/k8s 2>/dev/null || true"
    )


def _assert_k8s_metrics_phase(c, expected: str) -> None:
    body = _fetch_metrics_k8s(c)
    pattern = re.compile(
        rf'spur_k8s_cluster_phase{{[^}}]*phase="{expected}"[^}}]*}}\s+1'
    )
    assert pattern.search(body), (
        f"spur_k8s_cluster_phase for phase={expected} not 1 in:\n{body}"
    )


def _assert_k8s_metrics_cluster_up(c) -> None:
    body = _fetch_metrics_k8s(c)
    pattern = re.compile(r'spur_k8s_cluster_up\{[^}]*\}\s+1')
    assert pattern.search(body), (
        f"spur_k8s_cluster_up not 1 in:\n{body}"
    )


def _assert_spur_tracks_k0s(c) -> None:
    status = c.k8s_status()
    assert "members:" in status, (
        f"Spur lost k0s membership after restart:\n{status}"
    )
    nodes = c.sinfo_nodes()
    assert len(nodes) == len(c.node_names), (
        f"Spur lost track of nodes: expected {c.node_names}, "
        f"got {list(nodes.keys())}"
    )


def _k8s_node_roles(c) -> dict[str, str]:
    roles: dict[str, str] = {}
    for line in c.k8s_status().splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[1] in ("controller", "worker", "single"):
            roles[fields[0]] = fields[1]
    return roles


def _parse_nodes_by_role_metric(body: str) -> dict[str, int]:
    out: dict[str, int] = {}
    for m in re.finditer(
        r'spur_k8s_nodes_by_role\{[^}]*role="([^"]+)"[^}]*\}\s+(\d+)', body
    ):
        out[m.group(1)] = int(m.group(2))
    return out


def _assert_nodes_by_role_metric(c, expected: dict[str, int]) -> None:
    body = _fetch_metrics_k8s(c)
    actual = _parse_nodes_by_role_metric(body)
    assert actual == expected, (
        f"spur_k8s_nodes_by_role mismatch: {actual} != {expected}\n"
        f"metrics:\n{body}"
    )


def _restart_agent_and_wait(c, node_index: int) -> None:
    c.restart_agent(node_index=node_index)
    c.wait_agent_serving(node_index=node_index, timeout=60)


def _drain_and_restart_agent(c, node_index: int) -> None:
    node_name = c.node_names[node_index]

    members_pre = c.k8s_members()
    cp_pre = c.k8s_control_planes()
    etcd_pre = c.etcd_member_count()
    roles_pre = _k8s_node_roles(c)

    c.cli(["spur", "node", "drain", node_name, "--reason", "rolling-upgrade"])

    deadline = time.time() + 30
    while time.time() < deadline:
        nodes = c.sinfo_nodes()
        if node_name in nodes and "drain" in nodes[node_name]:
            break
        time.sleep(2)

    assert c.k8s_members() == members_pre, (
        f"drain of {node_name} changed k0s membership"
    )
    assert c.k8s_control_planes() == cp_pre, (
        f"drain of {node_name} changed control-plane list"
    )
    assert c.etcd_member_count() == etcd_pre, (
        f"drain of {node_name} changed etcd quorum: "
        f"{c.etcd_member_count()} != {etcd_pre}"
    )
    assert _k8s_node_roles(c) == roles_pre, (
        f"drain of {node_name} changed node roles: "
        f"{_k8s_node_roles(c)} != {roles_pre}"
    )

    _restart_agent_and_wait(c, node_index)


# --- WireGuard mesh assertions ----------------------------------------------


def _assert_mesh_intact(c) -> None:
    """Every node must still have its spur0 interface up with the expected peers,
    recent handshakes, overlay routes, and all-to-all reachability."""
    mesh: WgMesh = c.wg_mesh
    indices = c.wg_mesh_indices
    sudo = c._sudo_prefix()
    now = int(time.time())

    for i in indices:
        name = c.node_names[i]

        # Interface must be up
        iface_out = c.nodes[i].exec_allow_fail(
            f"{sudo}wg show '{WG_IFACE}' public-key 2>&1"
        ).strip()
        assert iface_out and "No such device" not in iface_out, (
            f"{name}: spur0 interface lost after restart"
        )

        # Peer count
        peers = c.nodes[i].exec_allow_fail(
            f"{sudo}wg show '{WG_IFACE}' peers"
        )
        peer_keys = {ln.strip() for ln in peers.splitlines() if ln.strip()}
        expected_peers = len(indices) - 1
        assert len(peer_keys) >= expected_peers, (
            f"{name}: expected >= {expected_peers} WG peers, "
            f"got {len(peer_keys)}"
        )

        # Handshake timestamps must be recent (non-zero, within 5 minutes)
        dump = c.nodes[i].exec_allow_fail(
            f"{sudo}wg show '{WG_IFACE}' dump"
        )
        for line in dump.splitlines()[1:]:
            fields = line.split("\t")
            if len(fields) < 8:
                continue
            peer_key = fields[0].strip()[:8]
            hs_str = fields[4].strip()
            if not hs_str or hs_str == "0":
                # Handshake not yet established — trigger with a ping and
                # let assert_all_to_all below retry
                continue
            hs_epoch = int(hs_str)
            age = now - hs_epoch
            assert age < 300, (
                f"{name}: peer {peer_key}… handshake {age}s old "
                f"(> 300s stale)"
            )

        # Overlay routes via spur0 must exist
        routes = c.nodes[i].exec_allow_fail(
            f"ip route show dev {WG_IFACE} 2>/dev/null"
        )
        assert routes.strip(), (
            f"{name}: no routes via {WG_IFACE} — overlay routing lost"
        )

    # All-to-all mesh ping (triggers handshakes for any lazy peers)
    mesh.assert_all_to_all(indices, settle_s=60)


def _assert_wg_keys_unchanged(c, keys_before: dict[int, str]) -> None:
    """WireGuard public keys must not change across restarts."""
    mesh: WgMesh = c.wg_mesh
    for i, key_before in keys_before.items():
        key_after = mesh.wg_pubkey(i)
        assert key_after == key_before, (
            f"node {c.node_names[i]}: WG key changed from {key_before} to "
            f"{key_after} — restart must re-adopt, not re-key"
        )


def _snapshot_wg_keys(c) -> dict[int, str]:
    mesh: WgMesh = c.wg_mesh
    return {i: mesh.wg_pubkey(i) for i in c.wg_mesh_indices}


# --- pod network assertions (ClusterIP + DNS) --------------------------------

_IPV4_RE = re.compile(r"^\d{1,3}(?:\.\d{1,3}){3}$")


def _launch_pinned_pod(c, cp_index: int, name: str, node_name: str) -> str:
    manifest = (
        "apiVersion: v1\n"
        "kind: Pod\n"
        f"metadata:\n  name: {name}\n"
        "spec:\n"
        f"  nodeName: {node_name}\n"
        "  containers:\n"
        f"  - name: {name}\n"
        f"    image: {BUSYBOX_IMAGE}\n"
        "    imagePullPolicy: IfNotPresent\n"
        "    command: ['sh','-c','sleep 3600']\n"
        "  restartPolicy: Never\n"
    )
    remote_path = f"/tmp/{name}.yaml"
    c.nodes[cp_index].write_file(remote_path, manifest)
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    assert "created" in out or "configured" in out or "unchanged" in out, (
        f"kubectl apply for pod {name} failed:\n{out}"
    )
    return name


def _launch_httpd_pod(c, cp_index: int, name: str, node_name: str,
                      body: str) -> str:
    manifest = (
        "apiVersion: v1\n"
        "kind: Pod\n"
        f"metadata:\n  name: {name}\n  labels:\n    app: {name}\n"
        "spec:\n"
        f"  nodeName: {node_name}\n"
        "  containers:\n"
        f"  - name: {name}\n"
        f"    image: {BUSYBOX_IMAGE}\n"
        "    imagePullPolicy: IfNotPresent\n"
        "    command: ['sh','-c',"
        f"'mkdir -p /w && printf {body} > /w/index.html && httpd -f -p 80 -h /w']\n"
        "    ports:\n    - containerPort: 80\n"
        "  restartPolicy: Never\n"
    )
    remote_path = f"/tmp/{name}.yaml"
    c.nodes[cp_index].write_file(remote_path, manifest)
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    assert "created" in out or "configured" in out or "unchanged" in out, (
        f"kubectl apply for httpd pod {name} failed:\n{out}"
    )
    return name


def _wait_pod_ip(c, cp_index: int, pod: str, timeout_s: int = 180) -> str:
    def read_ip() -> str:
        out = _kubectl(
            c, f"get pod {pod} -o jsonpath='{{.status.podIP}}'"
        ).strip()
        return out if _IPV4_RE.match(out) else ""
    wait_until(lambda: bool(read_ip()), timeout_s=timeout_s,
               desc=f"pod {pod} got a pod-CIDR IP")
    return read_ip()


def _expose_clusterip(c, cp_index: int, pod: str, svc: str,
                      port: int = 80) -> str:
    manifest = (
        "apiVersion: v1\n"
        "kind: Service\n"
        f"metadata:\n  name: {svc}\n"
        "spec:\n"
        "  type: ClusterIP\n"
        f"  selector:\n    app: {pod}\n"
        f"  ports:\n  - port: {port}\n    targetPort: {port}\n"
    )
    remote_path = f"/tmp/{svc}-svc.yaml"
    c.nodes[cp_index].write_file(remote_path, manifest)
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    assert "created" in out or "configured" in out or "unchanged" in out, (
        f"kubectl apply for service {svc} failed:\n{out}"
    )

    def read_cip() -> str:
        cip = _kubectl(
            c, f"get svc {svc} -o jsonpath='{{.spec.clusterIP}}'"
        ).strip()
        return cip if _IPV4_RE.match(cip) else ""
    wait_until(lambda: bool(read_cip()), timeout_s=60,
               desc=f"service {svc} got a ClusterIP")
    return read_cip()


def _assert_clusterip_reachable(c, cp_index: int, server_node: str,
                                client_node: str) -> None:
    """Deploy httpd on server_node, expose as ClusterIP, curl from client_node."""
    body = "SPUR_UPGRADE_SVC_OK"
    server = _launch_httpd_pod(c, cp_index, "upg-svc-server", server_node, body)
    _wait_pod_ip(c, cp_index, server)
    cluster_ip = _expose_clusterip(c, cp_index, server, "upg-svc", port=80)

    client = _launch_pinned_pod(c, cp_index, "upg-svc-client", client_node)
    _wait_pod_ip(c, cp_index, client)

    def fetched() -> bool:
        out = _kubectl(
            c,
            f"exec {client} -- wget -qO- --timeout=5 "
            f"http://{cluster_ip}:80 2>/dev/null"
        )
        return body in out

    wait_until(fetched, timeout_s=180,
               desc=f"ClusterIP {cluster_ip} reachable from {client_node}")

    for obj in ("pod/upg-svc-server", "pod/upg-svc-client", "svc/upg-svc"):
        _delete_k8s_resource(c, obj.split("/")[0], obj.split("/")[1])


def _assert_dns_resolves(c, cp_index: int, node_name: str) -> None:
    """Verify in-pod DNS resolution (kubernetes.default) works post-upgrade.

    CoreDNS on calico-over-WG can take a while to reconverge after restarts,
    so the timeout is generous. Uses wget as a secondary probe — busybox
    nslookup can be flaky on minimal images."""
    pod_name = "upg-dns-check"
    _launch_pinned_pod(c, cp_index, pod_name, node_name)
    _wait_pod_ip(c, cp_index, pod_name)

    def dns_ok() -> bool:
        out = _kubectl(
            c,
            f"exec {pod_name} -- nslookup kubernetes.default 2>&1"
        )
        if "Address" in out and "NXDOMAIN" not in out:
            return True
        # Fallback: wget to the k8s API (proves DNS + service routing)
        out2 = _kubectl(
            c,
            f"exec {pod_name} -- wget -qO- --timeout=3 "
            f"https://kubernetes.default:443/ 2>&1"
        )
        return "connection refused" not in out2.lower() and len(out2.strip()) > 0

    wait_until(dns_ok, timeout_s=180,
               desc="DNS nslookup kubernetes.default from pod")

    _delete_k8s_resource(c, "pod", pod_name)


# --- tests ------------------------------------------------------------------


@pytest.mark.k0s
class TestWgRollingUpgradePreservesK0s:
    """Prove that a rolling Spur upgrade (daemon restarts with drain semantics)
    does not disrupt a managed k0s cluster running over WireGuard (calico CNI,
    ``wg_enabled=true``).

    Identical structure to ``TestRollingUpgradePreservesK0s`` in
    ``test_upgrade_k0s.py``, but with the WireGuard mesh as the transport.
    After each upgrade step, additionally asserts:
    - The WireGuard mesh is intact (spur0 up, peers present, all-to-all ping)
    - WG public keys are unchanged (re-adopt, not re-key)
    - Calico reconverges on the mesh
    """

    def test_rolling_upgrade_preserves_k0s_over_wg(self, wg_k0s_cluster):
        c = wg_k0s_cluster
        cp_node = c.node_names[0]

        # --- bring k0s up and wait for readiness ---
        out = c.k8s_up(["--control-plane-node", cp_node])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        cp_set = set(c.k8s_control_planes())
        worker_names = [n for n in c.node_names if n not in cp_set]
        if not worker_names:
            worker_names = list(cp_set)

        cp_index = c.node_names.index(cp_node)

        # Wait for both k8s node readiness AND calico convergence — calico
        # over WG needs extra time for BGP peering over the mesh.
        wait_until(
            lambda: _k8s_nodes_ready(c, worker_names),
            timeout_s=300,
            desc="worker kubelets Ready",
        )
        wait_until(
            lambda: _calico_ready(c, cp_index),
            timeout_s=480,
            desc="calico-node DaemonSet Running cluster-wide",
        )

        if not _prepull_busybox(c):
            pytest.skip("cannot pre-pull busybox image (offline registry)")

        members_before = c.k8s_members()
        cp_list_before = c.k8s_control_planes()
        etcd_count_before = c.etcd_member_count()
        roles_before = _k8s_node_roles(c)
        nodes_by_role_before = _parse_nodes_by_role_metric(
            _fetch_metrics_k8s(c)
        )
        wg_keys_before = _snapshot_wg_keys(c)

        # --- deploy canary ---
        num_workers = len(c.node_names) - 1
        canary_replicas = max(num_workers, 1)
        _deploy_canary(c, "canary", canary_replicas)
        _wait_deployment_ready(c, "canary", timeout=300)
        canary_uids = _snapshot_pod_uids(c, "app=canary")

        # --- step 1: controller restart ---
        c.restart_controller()
        c.wait_k8s_phase("ready", timeout=120)

        _assert_spur_tracks_k0s(c)
        assert c.k8s_members() == members_before
        assert c.k8s_control_planes() == cp_list_before
        assert c.etcd_member_count() == etcd_count_before
        assert _k8s_node_roles(c) == roles_before
        _assert_canary_survived(c, "canary", canary_uids)
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)
        _assert_nodes_by_role_metric(c, nodes_by_role_before)

        # WG mesh must survive controller restart
        _assert_mesh_intact(c)
        _assert_wg_keys_unchanged(c, wg_keys_before)

        # --- step 2: rolling drain+restart of agents ---
        for i in range(len(c.nodes)):
            _drain_and_restart_agent(c, i)

        c.wait_k8s_phase("ready", timeout=120)
        wait_until(
            lambda: _k8s_nodes_ready(c, worker_names),
            timeout_s=120,
            desc="workers Ready after rolling restart",
        )
        # Calico may need to re-establish BGP sessions after agent restarts.
        wait_until(
            lambda: _calico_ready(c, cp_index),
            timeout_s=300,
            desc="calico-node reconverged after rolling agent restart",
        )

        _assert_spur_tracks_k0s(c)
        assert c.k8s_members() == members_before
        assert c.k8s_control_planes() == cp_list_before
        assert c.etcd_member_count() == etcd_count_before
        assert _k8s_node_roles(c) == roles_before
        _assert_canary_survived(c, "canary", canary_uids)
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)
        _assert_nodes_by_role_metric(c, nodes_by_role_before)

        # WG mesh must survive rolling agent restarts
        _assert_mesh_intact(c)
        _assert_wg_keys_unchanged(c, wg_keys_before)

        # --- step 3: pod network connectivity post-upgrade ---
        # ClusterIP service across nodes (kube-proxy DNAT over the mesh)
        if len(c.node_names) >= 3:
            _assert_clusterip_reachable(
                c, cp_index, c.node_names[1], c.node_names[2]
            )
        # DNS resolution from inside a pod
        worker_for_dns = worker_names[0] if worker_names else cp_node
        _assert_dns_resolves(c, cp_index, worker_for_dns)

        # --- step 4: new pod scheduling ---
        _schedule_new_pod(c, "post-upgrade-pod", timeout=180)

        # --- step 5: bring k8s down, verify batch scheduling ---
        _delete_k8s_resource(c, "deployment", "canary")
        _delete_k8s_resource(c, "pod", "post-upgrade-pod")
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)

        c.restart_controller()
        for i in range(len(c.nodes)):
            _restart_agent_and_wait(c, i)

        for name in c.node_names:
            c.cli_allow_fail(
                ["scontrol", "update", f"NodeName={name}", "State=RESUME"]
            )
        c.wait_ready(timeout=120)

        # WG mesh must still be intact after k0s teardown + daemon restarts
        _assert_mesh_intact(c)

        out_path = f"{c.remote_dir}/post-upgrade.out"
        script = c.write_file(
            "post-upgrade-job.sh",
            "#!/bin/bash\necho UPGRADE_OK\n",
            all_nodes=True,
        )
        out = c.sbatch(["--job-name=post-upgrade", "-o", out_path, script])
        from cluster import parse_job_id, wait_job

        job_id = parse_job_id(out)
        assert job_id is not None, f"sbatch did not return a job id: {out}"

        state = wait_job(c, job_id, timeout=120)
        if state not in ("CD", "GONE"):
            diag = c.cli_allow_fail(["scontrol", "show", "job", str(job_id)])
            assert False, (
                f"post-upgrade job {job_id} state {state}, expected CD\n{diag}"
            )

        output = c.read_output_on_any_node(out_path)
        assert "UPGRADE_OK" in output, (
            f"post-upgrade job output missing marker:\n{output}"
        )

    def test_wg_datapath_survives_upgrade(self, wg_k0s_cluster):
        """After a full rolling upgrade, cross-node pod traffic still rides the
        WireGuard tunnel (wg transfer counter rises during a pod-to-pod ping)."""
        c = wg_k0s_cluster
        if len(c.node_names) < 3:
            pytest.skip("datapath test needs >= 3 nodes (CP + two workers)")

        cp_node = c.node_names[0]
        worker_a, worker_b = c.node_names[1], c.node_names[2]

        out = c.k8s_up(["--control-plane-node", cp_node])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        cp_index = 0
        wait_until(
            lambda: _k8s_nodes_ready(c, [worker_a, worker_b]),
            timeout_s=300,
            desc="both workers Ready",
        )
        wait_until(
            lambda: _calico_ready(c, cp_index),
            timeout_s=480,
            desc="calico Running",
        )
        if not _prepull_busybox(c):
            pytest.skip("cannot pre-pull busybox image")

        # Full rolling upgrade
        c.restart_controller()
        c.wait_k8s_phase("ready", timeout=120)
        for i in range(len(c.nodes)):
            _drain_and_restart_agent(c, i)
        c.wait_k8s_phase("ready", timeout=120)
        wait_until(
            lambda: _k8s_nodes_ready(c, [worker_a, worker_b]),
            timeout_s=120,
            desc="workers Ready post-upgrade",
        )
        wait_until(
            lambda: _calico_ready(c, cp_index),
            timeout_s=300,
            desc="calico reconverged post-upgrade",
        )

        # Launch pods on distinct workers and verify WG tunnel carries the traffic
        pod_a = _launch_pinned_pod(c, cp_index, "wg-upg-a", worker_a)
        pod_b = _launch_pinned_pod(c, cp_index, "wg-upg-b", worker_b)
        _wait_pod_ip(c, cp_index, pod_a)
        ip_b = _wait_pod_ip(c, cp_index, pod_b)

        # Snapshot WG transfer counter for the worker_a→worker_b peer
        mesh: WgMesh = c.wg_mesh
        b_index = c.node_names.index(worker_b)
        a_index = c.node_names.index(worker_a)
        peer_key_b = mesh.wg_pubkey(b_index)
        before_rx, before_tx = mesh.wg_transfer(a_index, peer_key_b)

        # Ping pod_b from pod_a
        _kubectl(
            c,
            f"exec {pod_a} -- ping -c 10 -W 2 {ip_b} 2>/dev/null || true"
        )

        def counter_rose():
            rx, tx = mesh.wg_transfer(a_index, peer_key_b)
            return rx > before_rx or tx > before_tx

        wait_until(counter_rose, timeout_s=30,
                   desc="WG transfer counter rose (pod traffic rode the tunnel)")

        for pod in (pod_a, pod_b):
            _delete_k8s_resource(c, "pod", pod)


@pytest.mark.k0s
class TestWgVersionUpgradePreservesK0s:
    """Real version-to-version upgrade: start the cluster on baseline binaries
    (e.g. v0.14.0), bring k0s up, swap to the PR binaries, do a rolling
    restart, and prove k0s + WG mesh survived.

    Requires ``SPUR_TEST_BASELINE_BINARIES_DIR`` pointing to the old-version
    binaries. CI downloads these from the GitHub release; local runs can point
    to a pre-built directory. Skipped when unset.
    """

    def test_version_upgrade_preserves_k0s_over_wg(self, wg_k0s_cluster):
        baseline_dir = _get_baseline_binaries_dir()
        if not baseline_dir:
            pytest.skip(
                "SPUR_TEST_BASELINE_BINARIES_DIR not set — "
                "set it to a directory with v0.14.0 binaries to run "
                "the version upgrade test"
            )
        upgrade_dir = _get_upgrade_binaries_dir()

        c = wg_k0s_cluster

        try:
            # --- phase 1: start on baseline (old) binaries ---
            _swap_binaries(c, baseline_dir)
            c.restart_controller()
            for i in range(len(c.nodes)):
                _restart_agent_and_wait(c, i)
            c.wait_ready(timeout=120)

            cp_node = c.node_names[0]
            out = c.k8s_up(["--control-plane-node", cp_node])
            assert "provisioning requested" in out or "already" in out, out
            c.wait_k8s_phase("ready", timeout=600)

            cp_set = set(c.k8s_control_planes())
            worker_names = [n for n in c.node_names if n not in cp_set]
            if not worker_names:
                worker_names = list(cp_set)
            cp_index = c.node_names.index(cp_node)

            wait_until(
                lambda: _k8s_nodes_ready(c, worker_names),
                timeout_s=300,
                desc="workers Ready on baseline",
            )
            wait_until(
                lambda: _calico_ready(c, cp_index),
                timeout_s=480,
                desc="calico Running on baseline",
            )

            if not _prepull_busybox(c):
                pytest.skip("cannot pre-pull busybox image (offline registry)")

            members_before = c.k8s_members()
            cp_list_before = c.k8s_control_planes()
            etcd_count_before = c.etcd_member_count()
            roles_before = _k8s_node_roles(c)
            wg_keys_before = _snapshot_wg_keys(c)

            num_workers = len(c.node_names) - 1
            _deploy_canary(c, "canary", max(num_workers, 1))
            _wait_deployment_ready(c, "canary", timeout=300)
            canary_uids = _snapshot_pod_uids(c, "app=canary")

            # --- phase 2: swap to upgrade (new) binaries + rolling restart ---
            _swap_binaries(c, upgrade_dir)

            c.restart_controller()
            c.wait_k8s_phase("ready", timeout=120)

            _assert_spur_tracks_k0s(c)
            _assert_canary_survived(c, "canary", canary_uids)
            _assert_mesh_intact(c)
            _assert_wg_keys_unchanged(c, wg_keys_before)

            for i in range(len(c.nodes)):
                _drain_and_restart_agent(c, i)

            c.wait_k8s_phase("ready", timeout=120)
            wait_until(
                lambda: _k8s_nodes_ready(c, worker_names),
                timeout_s=120,
                desc="workers Ready after version upgrade",
            )
            wait_until(
                lambda: _calico_ready(c, cp_index),
                timeout_s=300,
                desc="calico reconverged after version upgrade",
            )

            _assert_spur_tracks_k0s(c)
            assert c.k8s_members() == members_before
            assert c.k8s_control_planes() == cp_list_before
            assert c.etcd_member_count() == etcd_count_before
            assert _k8s_node_roles(c) == roles_before
            _assert_canary_survived(c, "canary", canary_uids)
            _assert_k8s_metrics_phase(c, "ready")
            _assert_k8s_metrics_cluster_up(c)
            _assert_mesh_intact(c)
            _assert_wg_keys_unchanged(c, wg_keys_before)

            # Pod network connectivity after version upgrade
            if len(c.node_names) >= 3:
                _assert_clusterip_reachable(
                    c, cp_index, c.node_names[1], c.node_names[2]
                )
            dns_node = worker_names[0] if worker_names else cp_node
            _assert_dns_resolves(c, cp_index, dns_node)

            # --- phase 3: verify scheduling works on new version ---
            _schedule_new_pod(c, "post-version-upgrade-pod", timeout=180)

            _delete_k8s_resource(c, "deployment", "canary")
            _delete_k8s_resource(c, "pod", "post-version-upgrade-pod")
        finally:
            _swap_binaries(c, upgrade_dir)
