# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: online worker add/remove for the SPUR-managed k0s cluster, and membership drift.

Exercises the real CLI -> gRPC -> controller path for `spur k8s add-nodes` and
`spur k8s remove-nodes`, asserting membership scope + role assignment (which the
reconcile loop drives without k0s actually reaching `active`, exactly like
test_k8s_scheduling). Full k0s convergence is covered on hardware, not in CI.
"""

import time
from contextlib import contextmanager

import pytest

STRAY_UNIT = "/run/systemd/system/k0sworker.service"
STRAY_DROPIN = f"{STRAY_UNIT}.d/zz-e2e-stray.conf"


@pytest.fixture
def k8s_enabled_cluster(unstarted_cluster):
    """Cluster with `[cluster].enabled=true` so `spur k8s up`/`add-nodes` assign roles.
    Tears the managed cluster down on exit so no k0s state leaks into a later run."""
    unstarted_cluster.start(config_overrides={"cluster": {"enabled": True}})
    yield unstarted_cluster
    try:
        unstarted_cluster.k8s_down(reset=True)
    except Exception:
        pass


@pytest.fixture
def k8s_degraded_cluster(unstarted_cluster):
    """`k8s_enabled_cluster` with a 5 s provisioning timeout. k0s never reaches `active`
    here, and drift is only checked once the cluster has left `provisioning`."""
    unstarted_cluster.start(
        config_overrides={"cluster": {"enabled": True, "k8s_provisioning_timeout_secs": 5}}
    )
    yield unstarted_cluster
    try:
        unstarted_cluster.k8s_down(reset=True)
    except Exception:
        pass


@contextmanager
def stray_k0s_worker(cluster, node_index: int):
    """An active `k0sworker.service` that runs `sleep`, not k0s. spurd reads the unit's state
    by name, so the controller sees a stray worker without anything joining a cluster."""
    node, sudo = cluster.nodes[node_index], cluster._sudo_prefix()
    try:
        node.exec(f"{sudo}true")
    except RuntimeError:
        pytest.skip(f"a stray k0s unit needs sudo on {cluster.node_names[node_index]}")
    if "active" in node.exec_allow_fail("systemctl is-active k0sworker k0scontroller").split():
        pytest.skip(f"{cluster.node_names[node_index]} already runs a k0s unit")

    # The runtime unit serves a node with no k0sworker unit; on one that has it, the unit in
    # /etc wins and the drop-in replaces its ExecStart.
    try:
        node.exec(
            f"{sudo}sh -c \"mkdir -p {STRAY_UNIT}.d && "
            f"printf '[Service]\\nExecStart=/bin/sleep infinity\\n' > {STRAY_UNIT} && "
            f"printf '[Service]\\nType=simple\\nExecStart=\\nExecStart=/bin/sleep infinity\\n'"
            f" > {STRAY_DROPIN}\""
        )
        node.exec(f"{sudo}systemctl daemon-reload && {sudo}systemctl start k0sworker")
        yield
    finally:
        node.exec_allow_fail(
            f"{sudo}systemctl stop k0sworker; {sudo}rm -f {STRAY_UNIT} {STRAY_DROPIN}; "
            f"{sudo}rmdir {STRAY_UNIT}.d; {sudo}systemctl daemon-reload; "
            f"{sudo}systemctl reset-failed k0sworker"
        )


def _drift_lines(cluster) -> list[str]:
    return [line for line in cluster.spurctld_log().splitlines() if "k0s membership drift" in line]


def _wait_members(cluster, expected: list[str], timeout: int = 60) -> list[str]:
    """Poll `spur k8s status` until the member set equals `expected` (sorted)."""
    want = sorted(expected)
    deadline = time.time() + timeout
    last: list[str] = []
    while time.time() < deadline:
        last = sorted(cluster.k8s_member_list())
        if last == want:
            return last
        time.sleep(3)
    raise TimeoutError(
        f"member set never became {want}; last {last}:\n{cluster.k8s_status()}"
    )


class TestK8sOnlineMembership:
    def test_add_nodes_grows_a_scoped_cluster(self, k8s_enabled_cluster):
        """A scoped cluster started with one node grows to include a second via add-nodes,
        and the added node is assigned a k0s role by the reconcile loop (no down/reset)."""
        cluster = k8s_enabled_cluster
        if len(cluster.node_names) < 2:
            pytest.skip("add-nodes growth test needs >= 2 nodes")
        first, second = cluster.node_names[0], cluster.node_names[1]

        # Bring up scoped to just the first node; the second is registered but out of scope.
        cluster.k8s_up(["--nodes", first, "--control-plane-node", first])
        _wait_members(cluster, [first])

        # Grow the scope online.
        cluster.k8s_add_nodes(["--nodes", second])
        _wait_members(cluster, [first, second])

        # The reconcile loop assigns the newly in-scope node a role (worker), no re-up needed.
        deadline = time.time() + 90
        roled = False
        while time.time() < deadline:
            for line in cluster.k8s_status().splitlines():
                f = line.split()
                if len(f) >= 2 and f[0] == second and f[1] in ("worker", "controller", "single"):
                    roled = True
                    break
            if roled:
                break
            time.sleep(3)
        assert roled, f"added node {second} was never assigned a role:\n{cluster.k8s_status()}"

    def test_add_nodes_rejected_on_whole_inventory_cluster(self, k8s_enabled_cluster):
        """add-nodes on a whole-inventory cluster (no scope) is rejected — new nodes auto-enroll."""
        cluster = k8s_enabled_cluster
        target = cluster.node_names[0]
        cluster.k8s_up()  # no scope -> "all nodes"
        deadline = time.time() + 60
        while time.time() < deadline and cluster.k8s_members() != "all nodes":
            time.sleep(3)
        out = cluster.k8s_add_nodes(["--nodes", target])
        assert "NOT accepted" in out or "enrolls all nodes" in out, (
            f"add-nodes on a whole-inventory cluster should be rejected:\n{out}"
        )

    def test_remove_nodes_rejects_control_plane(self, k8s_enabled_cluster):
        """remove-nodes refuses the control-plane node (etcd-quorum change is out of scope).

        Scope to two members so removing the CP hits the control-plane guard, not the
        would-empty guard (which fires first when the CP is the only member)."""
        cluster = k8s_enabled_cluster
        if len(cluster.node_names) < 2:
            pytest.skip("control-plane rejection test needs >= 2 nodes (else would-empty fires first)")
        cp, worker = cluster.node_names[0], cluster.node_names[1]
        cluster.k8s_up(["--nodes", f"{cp},{worker}", "--control-plane-node", cp])
        _wait_members(cluster, [cp, worker])
        out = cluster.k8s_remove_nodes(["--nodes", cp])
        assert "NOT accepted" in out or "control plane" in out, (
            f"removing the control-plane node should be rejected:\n{out}"
        )

    def test_remove_nodes_rejects_non_member(self, k8s_enabled_cluster):
        """remove-nodes refuses a registered node that is not a member of the scoped cluster."""
        cluster = k8s_enabled_cluster
        if len(cluster.node_names) < 2:
            pytest.skip("non-member rejection test needs >= 2 nodes")
        first, second = cluster.node_names[0], cluster.node_names[1]
        cluster.k8s_up(["--nodes", first, "--control-plane-node", first])
        _wait_members(cluster, [first])
        out = cluster.k8s_remove_nodes(["--nodes", second])
        assert "NOT accepted" in out or "not a member" in out, (
            f"removing a non-member node should be rejected:\n{out}"
        )


class TestK8sMembershipDrift:
    def test_k0s_running_outside_a_scoped_cluster_is_reported(self, k8s_degraded_cluster):
        """Nodes without the stray unit, the member included, are never reported."""
        cluster = k8s_degraded_cluster
        if len(cluster.node_names) < 2:
            pytest.skip("drift test needs a node outside the scope")
        member, stray = cluster.node_names[0], cluster.node_names[1]
        cluster.k8s_up(["--nodes", member, "--control-plane-node", member])
        cluster.wait_k8s_phase("degraded", timeout=180)

        # Every node is a candidate and a tick visits one, so two visits take at most two
        # sweeps of 30 s per node.
        deadline = time.time() + 30 * (2 * len(cluster.node_names) + 2)
        reported: list[str] = []
        with stray_k0s_worker(cluster, 1):
            while time.time() < deadline:
                reported = [line for line in _drift_lines(cluster) if f"node={stray} " in line]
                if reported:
                    break
                time.sleep(10)

        assert reported, f"{stray} was never reported:\n{cluster.spurctld_log()[-4000:]}"
        assert 'kind="out_of_scope"' in reported[0], reported[0]
        others = [line for line in _drift_lines(cluster) if f"node={stray} " not in line]
        assert not others, f"drift reported on nodes without any: {others}"
