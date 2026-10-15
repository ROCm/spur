# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""`spur k8s` against a controller started without `[cluster] enabled`. The requests only
the reconcile loop acts on must be refused, not accepted and then never acted on."""

REFUSAL = "k0s management is not running on this controller"


class TestK8sWithoutReconcileLoop:
    def test_up_is_refused_and_nothing_is_recorded(self, cluster):
        out = cluster.k8s_up(["--nodes", cluster.node_names[0]])
        assert REFUSAL in out, f"up must be refused: {out}"
        assert "phase: down" in cluster.k8s_status()

    def test_add_nodes_is_refused(self, cluster):
        out = cluster.k8s_add_nodes(["--nodes", cluster.node_names[0]])
        assert REFUSAL in out, f"add-nodes must be refused: {out}"

    def test_down_is_accepted_while_no_node_holds_a_role(self, cluster):
        out = cluster.k8s_down(reset=False)
        assert "k0s cluster down requested" in out, f"down with no role held must go through: {out}"
