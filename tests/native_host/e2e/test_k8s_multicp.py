# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Multi-control-plane (HA) native k0s E2E tests (needs >= 3 nodes for etcd quorum)."""


class TestK8sMultiControlPlane:
    def test_three_control_planes_form_etcd_quorum(self, k8s_multicp_cluster):
        cluster = k8s_multicp_cluster

        out = cluster.k8s_up(["--replicas", "3"])
        assert "requested" in out or "up" in out.lower(), out

        cluster.wait_k8s_phase("ready", timeout=600)

        # Three distinct nodes were assigned the control-plane role.
        cps = cluster.k8s_control_planes()
        assert len(cps) == 3, f"expected 3 control planes, got {cps}"
        assert len(set(cps)) == 3, f"control planes not distinct: {cps}"

        # All three report an active controller component.
        active = cluster.k8s_active_controllers()
        assert len(active) == 3, f"expected 3 active controllers, got {active}"

        # Ground truth: the embedded etcd formed a real 3-member quorum.
        assert cluster.etcd_member_count() == 3, (
            f"expected a 3-member etcd quorum\n{cluster.k8s_status()}"
        )

    def test_even_replica_count_rejected(self, k8s_multicp_cluster):
        cluster = k8s_multicp_cluster
        out = cluster.cli_allow_fail(["spur", "k8s", "up", "--replicas", "2"])
        assert "1, 3, or 5" in out, f"even replica count must be rejected: {out}"
