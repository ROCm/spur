# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for retrying remote commands that fail on transient network trouble.

A fake node stands in for SshNode only; every test drives the real harness function.
"""

import pytest

import cluster as cluster_module
from cluster import exec_with_retry


class ScriptedNode:
    """Replays a scripted (exit_code, output) per call and records each command."""

    host = "node-a"

    def __init__(self, results):
        self.results = list(results)
        self.calls = []

    def exec_with_exit(self, command, timeout=None):
        self.calls.append((command, timeout))
        return self.results.pop(0)


@pytest.fixture
def sleeps(monkeypatch):
    recorded = []
    monkeypatch.setattr(cluster_module.time, "sleep", recorded.append)
    return recorded


def test_returns_output_of_first_success_without_retrying(sleeps):
    node = ScriptedNode([(0, "built")])

    assert exec_with_retry(node, "docker build x", what="build", attempts=3, timeout=900) == "built"
    assert node.calls == [("docker build x", 900)]
    assert sleeps == []


def test_retries_transient_failures_with_growing_backoff(sleeps):
    node = ScriptedNode([(1, "429 Too Many Requests"), (1, "archive.ubuntu.com timeout"), (0, "ok")])

    assert exec_with_retry(node, "cmd", what="build", attempts=3, timeout=900) == "ok"
    assert len(node.calls) == 3
    assert sleeps == [cluster_module.IMAGE_PULL_BACKOFF_SECS, cluster_module.IMAGE_PULL_BACKOFF_SECS * 2]


def test_final_failure_names_the_operation_and_the_last_output_tail(sleeps):
    lines = [f"step {i}" for i in range(200)] + ["toomanyrequests: 429 Too Many Requests"]
    node = ScriptedNode([(1, "first"), (1, "second"), (1, "\n".join(lines))])

    with pytest.raises(RuntimeError) as err:
        exec_with_retry(node, "cmd", what="docker build spur-attempt-cleanup", attempts=3, timeout=900)

    message = str(err.value)
    assert "docker build spur-attempt-cleanup failed on node-a after 3 attempts" in message
    assert "429 Too Many Requests" in message
    assert "step 199" in message
    assert "step 0\n" not in message
    assert "first" not in message
    assert len(sleeps) == 2
