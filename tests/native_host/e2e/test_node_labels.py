# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for node labels and partition selector routing."""

import time
import pytest

pytestmark = pytest.mark.suite_policy


def _wait_node_in_partition(cluster, node_name, partition, present=True, timeout=10):
    """Poll sinfo until node appears/disappears in partition."""
    deadline = time.time() + timeout
    members = set()
    while time.time() < deadline:
        members = cluster.nodes_in_partition(partition)
        if (node_name in members) == present:
            return members
        time.sleep(0.5)
    verb = "appear in" if present else "disappear from"
    assert False, f"{node_name} did not {verb} {partition} within {timeout}s:\n{members}"


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
        node0 = label_cluster.node_names[0]
        node1 = label_cluster.node_names[1]

        gpu_members = label_cluster.nodes_in_partition("gpu")

        assert node0 in gpu_members, (
            f"expected {node0} in gpu partition, members:\n{gpu_members}"
        )
        assert node1 not in gpu_members, (
            f"expected {node1} NOT in gpu partition, members:\n{gpu_members}"
        )

    def test_all_wildcard_includes_all_nodes(self, label_cluster):
        """The ALL-wildcard partition includes every node regardless of labels."""
        node0 = label_cluster.node_names[0]
        node1 = label_cluster.node_names[1]

        catchall_members = label_cluster.nodes_in_partition("catchall")

        assert node0 in catchall_members, (
            f"expected {node0} in catchall partition via ALL wildcard, members:\n{catchall_members}"
        )
        assert node1 in catchall_members, (
            f"expected {node1} in catchall partition via ALL wildcard, members:\n{catchall_members}"
        )

    def test_admin_label_update_reroutes_partition(self, label_cluster):
        """Adding a label via CLI routes the node into the partition; removing it unroutes."""
        node1 = label_cluster.node_names[1]

        # Node 1 should NOT be in gpu partition initially
        assert node1 not in label_cluster.nodes_in_partition("gpu"), (
            f"precondition: {node1} should not be in gpu partition"
        )

        # Add label → node joins gpu partition
        label_cluster.cli(["spur", "node", "label", node1, "gpu=mi300x"])
        _wait_node_in_partition(label_cluster, node1, "gpu", present=True)

        out = label_cluster.scontrol("show", "node", node1)
        assert "Labels=gpu=mi300x" in out, (
            f"after adding label, expected Labels=gpu=mi300x:\n{out}"
        )

        # Remove label → node leaves gpu partition
        label_cluster.cli(["spur", "node", "label", node1, "gpu-"])
        _wait_node_in_partition(label_cluster, node1, "gpu", present=False)
