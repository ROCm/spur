# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests proving a rolling Spur upgrade does not disrupt a managed
k0s cluster (no WireGuard).

Spur daemon restarts (simulating ``rolling_upgrade.yml``) must not break k0s —
the cluster stays ready, etcd quorum is preserved, deployed workloads survive
without pod restarts, and new workloads can be scheduled after the upgrade.

The tests use native k0s (kuberouter CNI, no WireGuard) matching the
production topology where CI clusters run without WireGuard.

Coverage split:
- **This file**: single-CP upgrade resilience, data-plane validation, drain
  semantics.
- ``test_k8s_multicp.py``: multi-CP HA (≥3 controllers, etcd quorum through
  rolling restarts). See ``TestK8sMultiControlPlane``.

See also ``TestAgentRestartPreservesRunningJob`` in ``test_deregistration.py``
for the no-k0s proof (job survival across agent restart).
"""

from __future__ import annotations

import re
import time

import pytest

from cluster import SpurCluster, make_remote_dir, parse_job_id, wait_job

BUSYBOX_IMAGE = "docker.io/library/busybox:1.36"

# --- helpers ----------------------------------------------------------------


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
    # k0s registers nodes with their FQDN; spur uses short hostnames —
    # match by prefix so "node-1" matches "node-1.example.com"
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
    k8s_status = c.k8s_status()
    raise TimeoutError(
        f"k8s nodes not all Ready within {timeout}s:\n"
        f"kubectl get nodes:\n{out}\n"
        f"spur k8s status:\n{k8s_status}"
    )


def _deploy_canary(c: SpurCluster, name: str, replicas: int) -> None:
    """Deploy a long-lived Deployment (busybox sleep) as a canary workload.
    The deployment spreads across workers so we can detect pod evictions."""
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
    """Assert that no pods matching the label selector have been restarted."""
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
    """Assert the canary deployment is fully ready with zero restarts."""
    _wait_deployment_ready(c, name, timeout=120)
    _assert_no_pod_restarts(c, f"app={name}")


def _schedule_new_pod(c: SpurCluster, name: str, timeout: int = 180) -> None:
    """Deploy a new pod and wait for it to reach Running — proves the scheduler
    and kubelet are functional post-upgrade."""
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


def _delete_k8s_resource(c: SpurCluster, kind: str, name: str) -> None:
    _kubectl(c, f"delete {kind} {name} --ignore-not-found 2>/dev/null || true")


def _prepull_busybox(c: SpurCluster) -> bool:
    """Pre-pull busybox into k0s's containerd namespace on every worker.
    Tries k0s ctr pull, then falls back to importing a pre-staged tarball.
    Skips nodes where k0s containerd isn't running (controller-only)."""
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


def _fetch_metrics_k8s(c: SpurCluster) -> str:
    """Fetch /metrics/k8s from spurctld's metrics endpoint via the controller
    node (metrics bind to loopback by default)."""
    return c.nodes[0].exec_allow_fail(
        "curl -sf http://127.0.0.1:6822/metrics/k8s 2>/dev/null || true"
    )


def _assert_k8s_metrics_phase(c: SpurCluster, expected: str) -> None:
    """Assert that spur_k8s_cluster_phase metric shows the expected phase=1."""
    body = _fetch_metrics_k8s(c)
    pattern = re.compile(
        rf'spur_k8s_cluster_phase{{[^}}]*phase="{expected}"[^}}]*}}\s+1'
    )
    assert pattern.search(body), (
        f"spur_k8s_cluster_phase for phase={expected} not 1 in:\n{body}"
    )


def _assert_k8s_metrics_cluster_up(c: SpurCluster) -> None:
    """Assert spur_k8s_cluster_up=1 (the primary alerting gauge)."""
    body = _fetch_metrics_k8s(c)
    pattern = re.compile(r'spur_k8s_cluster_up\{[^}]*\}\s+1')
    assert pattern.search(body), (
        f"spur_k8s_cluster_up not 1 in:\n{body}"
    )


def _assert_spur_tracks_k0s(c: SpurCluster) -> None:
    """Assert Spur still knows about the k0s cluster (phase, members)."""
    status = c.k8s_status()
    assert "members:" in status, (
        f"Spur lost k0s membership after restart:\n{status}"
    )
    nodes = c.sinfo_nodes()
    assert len(nodes) == len(c.node_names), (
        f"Spur lost track of nodes: expected {c.node_names}, "
        f"got {list(nodes.keys())}"
    )


def _k8s_node_roles(c: SpurCluster) -> dict[str, str]:
    """Parse per-node roles from ``spur k8s status`` output.

    Returns {node_name: role} where role is controller/worker/single."""
    roles: dict[str, str] = {}
    for line in c.k8s_status().splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[1] in ("controller", "worker", "single"):
            roles[fields[0]] = fields[1]
    return roles


def _parse_nodes_by_role_metric(body: str) -> dict[str, int]:
    """Parse spur_k8s_nodes_by_role{...,role="X"} N lines into {role: count}."""
    out: dict[str, int] = {}
    for m in re.finditer(
        r'spur_k8s_nodes_by_role\{[^}]*role="([^"]+)"[^}]*\}\s+(\d+)', body
    ):
        out[m.group(1)] = int(m.group(2))
    return out


def _assert_nodes_by_role_metric(c: SpurCluster,
                                 expected: dict[str, int]) -> None:
    """Assert spur_k8s_nodes_by_role matches the expected role counts."""
    body = _fetch_metrics_k8s(c)
    actual = _parse_nodes_by_role_metric(body)
    assert actual == expected, (
        f"spur_k8s_nodes_by_role mismatch: {actual} != {expected}\n"
        f"metrics:\n{body}"
    )


def _restart_agent_and_wait(c: SpurCluster, node_index: int) -> None:
    node_name = c.node_names[node_index]
    c.restart_agent(node_index=node_index)
    deadline = time.time() + 60
    while time.time() < deadline:
        if node_name in c.sinfo_nodes():
            return
        time.sleep(2)
    raise AssertionError(
        f"{node_name} did not re-register within 60s after agent restart"
    )


def _drain_and_restart_agent(c: SpurCluster, node_index: int) -> None:
    """Drain a node (matching rolling_upgrade.yml), restart spurd, wait for
    re-registration.  The agent re-registers as idle after restart, so no
    explicit resume is needed."""
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

    # Drain must NOT cascade into a k0s removal — membership, CPs, etcd
    # quorum, and per-node roles must be unchanged while drained.
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


def _reset_k0s_all_nodes(c: SpurCluster) -> None:
    """Forcibly reset k0s on every node (matches conftest._reset_k0s_all_nodes)."""
    sudo = c._sudo_prefix()
    for node in c.nodes:
        node.exec_allow_fail(
            f"{sudo}systemctl stop k0scontroller k0sworker 2>/dev/null || true"
        )
        node.exec_allow_fail(f"{sudo}k0s stop 2>/dev/null || true")
        node.exec_allow_fail(f"{sudo}k0s reset 2>/dev/null || true")
        node.exec_allow_fail(
            f"{sudo}rm -rf /etc/k0s /var/lib/k0s /run/k0s 2>/dev/null || true"
        )


# --- fixture ----------------------------------------------------------------


@pytest.fixture
def k0s_native_cluster(ssh_nodes, remote_bin_dir):
    """Native k0s cluster (no WireGuard, kuberouter CNI), rootful, >= 3 nodes.

    Stands up a Spur cluster with k0s enabled but does NOT call ``k8s up`` —
    the test controls timing so it can assert state before and after each
    upgrade step.
    """
    if len(ssh_nodes) < 3:
        pytest.skip(
            f"upgrade k0s tests require >= 3 nodes for etcd quorum "
            f"(got {len(ssh_nodes)})"
        )
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    c.provision()
    c.root_agent_preflight()
    _reset_k0s_all_nodes(c)
    try:
        c.start(
            config_overrides={
                "cluster": {"enabled": True, "cni": "kuberouter"},
                "auth": {"allow_root_jobs": True},
            },
            agent_as_root=True,
        )
    except Exception:
        c.teardown()
        raise
    yield c
    try:
        _delete_k8s_resource(c, "deployment", "canary")
        _delete_k8s_resource(c, "pod", "post-upgrade-pod")
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)
    except Exception:
        pass
    c.teardown()
    _reset_k0s_all_nodes(c)


# --- tests ------------------------------------------------------------------


@pytest.mark.k0s
class TestRollingUpgradePreservesK0s:
    """Prove that a rolling Spur upgrade (daemon restarts with drain semantics)
    does not disrupt a managed k0s cluster running without WireGuard.

    The sequence mirrors ``rolling_upgrade.yml``:
    1. Bring k0s up, wait for the spur-level ``ready`` phase and k8s node
       readiness.
    2. Deploy a canary workload (Deployment with replicas) as data-plane probe.
    3. Restart spurctld — re-poll for fresh ready state, verify phase,
       membership, control-planes, etcd quorum, canary survival, and metrics.
    4. Rolling drain+restart of each spurd (one at a time) — same assertions.
    5. Verify new pod scheduling works (k8s scheduler functional).
    6. Bring k0s down, then submit a batch job to verify Spur scheduling still
       works (batch and k8s coexistence).
    """

    def test_rolling_upgrade_preserves_k0s_and_scheduling(
        self, k0s_native_cluster
    ):
        c = k0s_native_cluster
        cp_node = c.node_names[0]
        multi_node = len(c.node_names) > 1

        # --- bring k0s up and wait for spur-level + k8s-level ready ---
        up_args = ["--control-plane-node", cp_node] if multi_node else []
        out = c.k8s_up(up_args)
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        # pure controllers don't run kubelet and won't appear in
        # `kubectl get nodes`; single-node mode runs both roles
        cp_set = set(c.k8s_control_planes())
        worker_names = [n for n in c.node_names if n not in cp_set]
        if not worker_names:
            worker_names = list(cp_set)
        _wait_k8s_nodes_ready(c, worker_names, timeout=300)

        # pre-pull busybox so pod scheduling doesn't block on image pull
        if not _prepull_busybox(c):
            pytest.skip("cannot pre-pull busybox image (offline registry)")

        members_before = c.k8s_members()
        cp_list_before = c.k8s_control_planes()
        etcd_count_before = c.etcd_member_count()
        roles_before = _k8s_node_roles(c)
        nodes_by_role_before = _parse_nodes_by_role_metric(_fetch_metrics_k8s(c))

        # --- deploy canary workload for data-plane validation ---
        num_workers = len(c.node_names) - 1
        canary_replicas = max(num_workers, 1)
        _deploy_canary(c, "canary", canary_replicas)
        _wait_deployment_ready(c, "canary", timeout=300)

        # --- step 1: controller restart (rolling_upgrade play 3) ---
        c.restart_controller()

        # re-poll for fresh reconciliation (fixes stale-read race)
        c.wait_k8s_phase("ready", timeout=120)

        _assert_spur_tracks_k0s(c)
        assert c.k8s_members() == members_before, (
            "k0s membership changed after controller restart"
        )
        assert c.k8s_control_planes() == cp_list_before, (
            f"control-plane list changed: {c.k8s_control_planes()} "
            f"!= {cp_list_before}"
        )
        assert c.etcd_member_count() == etcd_count_before, (
            f"etcd quorum lost: {c.etcd_member_count()} != {etcd_count_before}"
        )
        assert _k8s_node_roles(c) == roles_before, (
            f"node roles changed after controller restart: "
            f"{_k8s_node_roles(c)} != {roles_before}"
        )
        _assert_canary_survived(c, "canary")

        # metrics re-converge after controller restart
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)
        _assert_nodes_by_role_metric(c, nodes_by_role_before)

        # --- step 2: rolling drain+restart of agents (rolling_upgrade play 4) ---
        for i in range(len(c.nodes)):
            _drain_and_restart_agent(c, i)

        # re-poll after all agent restarts
        c.wait_k8s_phase("ready", timeout=120)
        _wait_k8s_nodes_ready(c, worker_names, timeout=120)

        _assert_spur_tracks_k0s(c)
        assert c.k8s_members() == members_before, (
            "k0s membership changed after rolling agent restart"
        )
        assert c.k8s_control_planes() == cp_list_before, (
            f"control-plane list changed after agent restart: "
            f"{c.k8s_control_planes()} != {cp_list_before}"
        )
        assert c.etcd_member_count() == etcd_count_before, (
            f"etcd quorum lost after agent restart: "
            f"{c.etcd_member_count()} != {etcd_count_before}"
        )
        assert _k8s_node_roles(c) == roles_before, (
            f"node roles changed after rolling agent restart: "
            f"{_k8s_node_roles(c)} != {roles_before}"
        )
        _assert_canary_survived(c, "canary")
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)
        _assert_nodes_by_role_metric(c, nodes_by_role_before)

        # --- step 3: new pod scheduling (post-upgrade k8s data-plane) ---
        _schedule_new_pod(c, "post-upgrade-pod", timeout=180)

        # --- step 4: bring k8s down, then verify batch scheduling ---
        _delete_k8s_resource(c, "deployment", "canary")
        _delete_k8s_resource(c, "pod", "post-upgrade-pod")
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)

        # k0s reset may leave nodes "down" — restart controller + agents
        # so the cluster re-forms cleanly for batch scheduling
        c.restart_controller()
        for i in range(len(c.nodes)):
            _restart_agent_and_wait(c, i)

        # k0s teardown can mark nodes "down" in Slurm (non-responsive
        # during the reset window). Resume them explicitly so batch
        # scheduling can proceed.
        for name in c.node_names:
            c.cli_allow_fail(
                ["scontrol", "update", f"NodeName={name}", "State=RESUME"]
            )
        c.wait_ready(timeout=120)

        out_path = f"{c.remote_dir}/post-upgrade.out"
        script = c.write_file(
            "post-upgrade-job.sh",
            "#!/bin/bash\necho UPGRADE_OK\n",
            all_nodes=True,
        )
        out = c.sbatch(["--job-name=post-upgrade", "-o", out_path, script])
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

    def test_worker_node_cp_survives_restart(self, k0s_native_cluster):
        """CP placed on a worker node (not co-located with spurctld) survives
        controller + agent restart.  Mirrors the common production layout
        where --control-plane-node picks a spurd-only node."""
        c = k0s_native_cluster

        # Place CP on node[1] (a worker), not node[0] (the controller host).
        cp_node = c.node_names[1]
        out = c.k8s_up(["--control-plane-node", cp_node])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        cps = c.k8s_control_planes()
        assert cp_node in cps, (
            f"expected {cp_node} as CP, got {cps}"
        )

        roles_before = _k8s_node_roles(c)
        members_before = c.k8s_members()
        etcd_before = c.etcd_member_count()

        # Controller restart — CP runs on a different node (spurd-only).
        c.restart_controller()
        c.wait_k8s_phase("ready", timeout=120)

        assert c.k8s_control_planes() == cps, (
            f"CP list changed: {c.k8s_control_planes()} != {cps}"
        )
        assert _k8s_node_roles(c) == roles_before, (
            f"roles changed: {_k8s_node_roles(c)} != {roles_before}"
        )
        assert c.k8s_members() == members_before
        assert c.etcd_member_count() == etcd_before
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)

        # Restart the agent on the CP node itself.
        cp_idx = c.node_names.index(cp_node)
        _restart_agent_and_wait(c, cp_idx)
        c.wait_k8s_phase("ready", timeout=120)

        assert c.k8s_control_planes() == cps, (
            f"CP list changed after CP-node agent restart: "
            f"{c.k8s_control_planes()} != {cps}"
        )
        assert _k8s_node_roles(c) == roles_before
        assert c.etcd_member_count() == etcd_before
        _assert_k8s_metrics_phase(c, "ready")
        _assert_k8s_metrics_cluster_up(c)
