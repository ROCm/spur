# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for node labels and partition selector routing."""

import time


class TestNodeLabels:
    """Node label registration, selector routing, and admin mutation."""

    def test_agent_registers_with_labels(self, label_cluster):
        """Labels passed via --label appear in scontrol show node output."""
        node_name = label_cluster.node_names[0]
        out = label_cluster.scontrol("show", "node", node_name)
        assert "Labels=gpu=mi300x" in out, (
            f"expected Labels=gpu=mi300x in scontrol output for {node_name}:\n{out}"
        )

    def test_selector_partition_routes_labeled_node(self, label_cluster):
        """Only the labeled node appears in the selector-based partition."""
        out = label_cluster.sinfo()
        node0 = label_cluster.node_names[0]
        node1 = label_cluster.node_names[1]

        gpu_lines = [l for l in out.splitlines() if "gpu" in l.split()[0:1]]
        gpu_text = "\n".join(gpu_lines)

        assert node0 in gpu_text, (
            f"expected {node0} in gpu partition, sinfo:\n{out}"
        )
        assert node1 not in gpu_text, (
            f"expected {node1} NOT in gpu partition, sinfo:\n{out}"
        )

    def test_all_wildcard_includes_all_nodes(self, label_cluster):
        """The ALL-wildcard partition includes every node regardless of labels."""
        out = label_cluster.sinfo()
        node0 = label_cluster.node_names[0]
        node1 = label_cluster.node_names[1]

        catchall_lines = [l for l in out.splitlines() if l.split() and l.split()[0].startswith("catchall")]
        catchall_text = "\n".join(catchall_lines)

        assert node0 in catchall_text, (
            f"expected {node0} in catchall partition via ALL wildcard, sinfo:\n{out}"
        )
        assert node1 in catchall_text, (
            f"expected {node1} in catchall partition via ALL wildcard, sinfo:\n{out}"
        )

    def test_admin_label_update_reroutes_partition(self, label_cluster):
        """Adding a label via CLI routes the node into the partition; removing it unroutes."""
        node1 = label_cluster.node_names[1]

        # Node 1 should NOT be in gpu partition initially
        out = label_cluster.sinfo()
        gpu_lines = [l for l in out.splitlines() if "gpu" in l.split()[0:1]]
        gpu_text = "\n".join(gpu_lines)
        assert node1 not in gpu_text, (
            f"precondition: {node1} should not be in gpu partition:\n{out}"
        )

        # Add label → node joins gpu partition
        label_cluster.cli(["spur", "node", "label", node1, "gpu=mi300x"])
        time.sleep(2)

        out = label_cluster.scontrol("show", "node", node1)
        assert "Labels=gpu=mi300x" in out, (
            f"after adding label, expected Labels=gpu=mi300x:\n{out}"
        )
        out = label_cluster.sinfo()
        gpu_lines = [l for l in out.splitlines() if "gpu" in l.split()[0:1]]
        gpu_text = "\n".join(gpu_lines)
        assert node1 in gpu_text, (
            f"after adding label, expected {node1} in gpu partition:\n{out}"
        )

        # Remove label → node leaves gpu partition
        label_cluster.cli(["spur", "node", "label", node1, "gpu-"])
        time.sleep(2)

        out = label_cluster.sinfo()
        gpu_lines = [l for l in out.splitlines() if "gpu" in l.split()[0:1]]
        gpu_text = "\n".join(gpu_lines)
        assert node1 not in gpu_text, (
            f"after removing label, expected {node1} NOT in gpu partition:\n{out}"
        )
