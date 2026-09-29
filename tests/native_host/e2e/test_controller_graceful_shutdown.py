# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""SIGTERM must stop spurctld within shutdown_grace_secs, even while an srun
step holds its RunStep RPC open for the step's whole runtime."""

import re
import shlex
import time

import pytest

SHUTDOWN_GRACE_SECS = 5
# Slack for SSH round-trips and process teardown: a bounded drain exits after
# about SHUTDOWN_GRACE_SECS, an unbounded one only once the step ends.
SHUTDOWN_DEADLINE_SECS = SHUTDOWN_GRACE_SECS + 20
STEP_SECS = 60


def _controller_pid(cluster) -> str:
    # The bracket keeps pgrep from matching the shell that runs it.
    pids = cluster.nodes[0].exec(f"pgrep -f '{cluster.bin_dir}/[s]purctld'").split()
    assert len(pids) == 1, f"expected one spurctld, found {pids}"
    return pids[0]


def _alive(node, pid: str) -> bool:
    return node.exec_allow_fail(f"test -d /proc/{pid} && echo alive").strip() == "alive"


def _uptime_secs(node, pid: str) -> int:
    return int(node.exec(f"ps -o etimes= -p {pid}").strip())


def _wait_for_file(node, path: str, timeout: float):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if node.exec_allow_fail(f"test -e {shlex.quote(path)} && echo yes").strip() == "yes":
            return
        time.sleep(0.5)
    raise TimeoutError(f"{path} did not appear within {timeout}s")


class TestControllerGracefulShutdown:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"shutdown_grace_secs": SHUTDOWN_GRACE_SECS}}

    def test_sigterm_does_not_wait_for_an_active_srun_step(self, cluster):
        node = cluster.nodes[0]
        started = f"{cluster.remote_dir}/step-started"
        # The workload drops the marker itself: a job reaching R precedes the
        # RunStep dispatch, so job state alone does not prove the RPC is open.
        step = f"touch {shlex.quote(started)}; exec sleep {STEP_SECS}"
        srun = cluster._cli_env_assignments() + [
            "nohup",
            shlex.quote(f"{cluster.bin_dir}/srun"),
            "-J",
            "shutdown-hold",
            "-w",
            shlex.quote(cluster.node_names[0]),
            "bash",
            "-c",
            shlex.quote(step),
            ">",
            shlex.quote(f"{cluster.remote_dir}/shutdown-hold.log"),
            "2>&1",
            "&",
        ]
        node.exec(" ".join(srun))
        _wait_for_file(node, started, timeout=60)

        pid = _controller_pid(cluster)
        try:
            # A deadline counted from startup instead of from SIGTERM would
            # already have stopped the controller by now.
            time.sleep(max(0, SHUTDOWN_GRACE_SECS + 2 - _uptime_secs(node, pid)))
            assert _alive(node, pid), "spurctld stopped on its own before any SIGTERM"

            start = time.monotonic()
            node.exec(f"kill -TERM {pid}")
            while _alive(node, pid) and time.monotonic() - start < SHUTDOWN_DEADLINE_SECS:
                time.sleep(0.5)
            elapsed = time.monotonic() - start

            assert not _alive(node, pid), (
                f"spurctld still running {elapsed:.0f}s after SIGTERM: its drain is "
                "waiting on the active srun step"
            )
            assert elapsed >= SHUTDOWN_GRACE_SECS, (
                f"spurctld exited {elapsed:.1f}s after SIGTERM, inside the grace period: "
                "the step's RPC was not in flight, so the bounded drain was never exercised"
            )
            log = re.sub(r"\x1b\[[0-9;]*m", "", node.read_file(f"{cluster.log_dir}/spurctld.log"))
            assert "graceful shutdown drain exceeded" in log, log[-2000:]
        finally:
            # A controller stuck draining ignores the SIGTERM the fixture teardown sends.
            node.exec_allow_fail(f"kill -9 {pid} 2>/dev/null || true")
