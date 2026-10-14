# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""`spur k8s` against a controller started without `[cluster] enabled`. The requests only
the reconcile loop acts on must be refused, not accepted and then never acted on."""

REFUSAL = "k0s management is not running on this controller"


class TestK8sWithoutReconcileLoop:
    def test_up_is_refused_and_nothing_is_recorded(self, cluster):
        out = cluster.k8s_up(["--nodes", cluster.node_names[0]])
        assert REFUSAL in out, f"up must be refused: {out}"

        status = cluster.cli_as_user("root", ["spur", "k8s", "status"])
        assert "phase: down" in status, f"a refused up must record nothing: {status}"

    def test_down_and_add_nodes_are_refused(self, cluster):
        down = cluster.k8s_down(reset=False)
        assert REFUSAL in down, f"down must be refused: {down}"

        add = cluster.k8s_add_nodes(["--nodes", cluster.node_names[0]])
        assert REFUSAL in add, f"add-nodes must be refused: {add}"
