# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests for `cni=calico` with the WireGuard mesh OFF.

Calico's mesh-over-WireGuard scenarios live in `test_wg_k0s.py`; this file
covers the other, non-mesh mode: the k0s API advertised on each node's real
underlay address and Calico running its own `vxlan` overlay instead of `bird`
native routing. Previously this combination hung forever (the API tried to
bind a WireGuard mesh IP that was never assigned to any interface).
"""

from __future__ import annotations

import re

import pytest

from wg_cluster import wait_until

_IPV4_RE = re.compile(r"^\d{1,3}(?:\.\d{1,3}){3}$")


def _internal_ips(c, cp_index: int) -> set[str]:
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl get nodes "
        "-o jsonpath='{range .items[*]}{.status.addresses[?(@.type==\"InternalIP\")].address}{\"\\n\"}{end}'"
    )
    return {t for t in (line.strip() for line in out.splitlines()) if _IPV4_RE.match(t)}


def _active_node_names(c) -> set[str]:
    """Every node name whose `spur k8s status` row reports `active` — control
    plane or worker. `phase` alone reaches `ready` on control-plane quorum
    alone (partial-ready), so a stuck worker doesn't show up there; this does."""
    names = set()
    for line in c.k8s_status().splitlines():
        fields = line.split()
        if len(fields) >= 3 and fields[2] == "active":
            names.add(fields[0])
    return names


@pytest.mark.k0s
class TestCalicoWithoutMesh:
    def test_cluster_reaches_ready_without_a_mesh(self, k8s_calico_direct_cluster):
        c = k8s_calico_direct_cluster
        out = c.k8s_up(["--control-plane-node", c.node_names[0]])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

        # `ready` alone only proves control-plane quorum (partial-ready) -- the
        # bug this fixture exercises left the worker stuck `inactive` forever
        # even after that. Every node must actually become active.
        wait_until(lambda: set(c.node_names) <= _active_node_names(c), timeout_s=180,
                   desc="every node (not just the control plane) becomes active")

    def test_node_internal_ip_is_the_real_underlay_address(self, k8s_calico_direct_cluster):
        """Without a mesh, kubelet's --node-ip must be the node's real address,
        not a WireGuard mesh IP that was never bound to any interface."""
        c = k8s_calico_direct_cluster
        c.k8s_up(["--control-plane-node", c.node_names[0]])
        c.wait_k8s_phase("ready", timeout=600)

        cps = c.k8s_control_planes()
        assert cps, "no control-plane node reported"
        cp_index = c.node_names.index(cps[0])

        wait_until(lambda: len(_internal_ips(c, cp_index)) > 0, timeout_s=180,
                   desc="kubelet InternalIPs registered with the k8s API")

        underlay_hosts = {n.host for n in c.nodes}
        internal_ips = _internal_ips(c, cp_index)
        assert internal_ips <= underlay_hosts, (
            f"InternalIPs must be real underlay addresses, got {internal_ips} "
            f"(expected a subset of {underlay_hosts})"
        )

    def test_calico_pods_come_up_in_vxlan_mode(self, k8s_calico_direct_cluster):
        c = k8s_calico_direct_cluster
        c.k8s_up(["--control-plane-node", c.node_names[0]])
        c.wait_k8s_phase("ready", timeout=600)
        cps = c.k8s_control_planes()
        cp_index = c.node_names.index(cps[0])

        def calico_node_running() -> bool:
            out = c.nodes[cp_index].exec_allow_fail(
                f"{c._sudo_prefix()}k0s kubectl get pods -n kube-system "
                "-l k8s-app=calico-node --no-headers 2>/dev/null"
            )
            lines = [ln for ln in out.splitlines() if ln.strip()]
            return bool(lines) and all(
                len(f := ln.split()) >= 3 and f[1] == "1/1" and f[2] == "Running"
                for ln in lines
            )

        wait_until(calico_node_running, timeout_s=180,
                   desc="calico-node DaemonSet Running (vxlan mode, no mesh)")

        cfg = c.nodes[cp_index].exec_allow_fail(
            f"{c._sudo_prefix()}cat /etc/k0s/k0s.yaml"
        )
        assert "mode: vxlan" in cfg, f"expected vxlan mode without a mesh, got:\n{cfg}"
