# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""SIGTERM must stop spurctld within shutdown_grace_secs, even while an srun
step holds its RunStep RPC open for the step's whole runtime, and the step
must survive the restart that follows."""

import re
import shlex
import time

import pytest

from cluster import wait_until

SHUTDOWN_GRACE_SECS = 5
# Slack for SSH round-trips and process teardown: a bounded drain exits after
# about SHUTDOWN_GRACE_SECS, an unbounded one only once the step ends.
SHUTDOWN_DEADLINE_SECS = SHUTDOWN_GRACE_SECS + 20
STEP_SECS = 40


def _controller_pid(cluster) -> str:
    # The bracket keeps pgrep from matching the shell that runs it.
    pids = cluster.nodes[0].exec(f"pgrep -f '{cluster.bin_dir}/[s]purctld'").split()
    assert len(pids) == 1, f"expected one spurctld, found {pids}"
    return pids[0]


def _alive(node, pid: str) -> bool:
    return node.exec_allow_fail(f"test -d /proc/{pid} && echo alive").strip() == "alive"


def _uptime_secs(node, pid: str) -> int:
    return int(node.exec(f"ps -o etimes= -p {pid}").strip())


def _srun_finished(node, log: str) -> str | None:
    text = node.read_file(log)
    return text if "srun-exit=" in text else None


class TestControllerGracefulShutdown:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"shutdown_grace_secs": SHUTDOWN_GRACE_SECS}}

    def test_sigterm_does_not_wait_for_an_active_srun_step(self, cluster):
        node = cluster.nodes[0]
        started = f"{cluster.remote_dir}/step-started"
        log = f"{cluster.remote_dir}/shutdown-hold.log"
        # The workload drops the marker itself: a job reaching R precedes the
        # RunStep dispatch, so job state alone does not prove the RPC is open.
        step = f"echo started >> {shlex.quote(started)}; sleep {STEP_SECS}"
        srun = " ".join(
            cluster._cli_env_assignments()
            + [
                shlex.quote(f"{cluster.bin_dir}/srun"),
                "-J",
                "shutdown-hold",
                "-w",
                shlex.quote(cluster.node_names[0]),
                "bash",
                "-c",
                shlex.quote(step),
            ]
        )
        wrapper = shlex.quote(f"{srun}; echo srun-exit=$?")
        node.exec(f"nohup sh -c {wrapper} > {shlex.quote(log)} 2>&1 &")
        wait_until(lambda: node.read_file(started), timeout_s=60, interval_s=1, desc="the step to start")

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
            # Read before the restart below truncates the log.
            ctl_log = re.sub(r"\x1b\[[0-9;]*m", "", node.read_file(f"{cluster.log_dir}/spurctld.log"))
            assert "graceful shutdown drain exceeded" in ctl_log, ctl_log[-2000:]
            # Logged only after main's accounting flush, so an exit that skips it fails here.
            assert "spurctld stopped" in ctl_log, ctl_log[-2000:]

            cluster.restart_controller()
            out = wait_until(
                lambda: _srun_finished(node, log),
                timeout_s=STEP_SECS + 60,
                interval_s=2,
                desc="srun to re-attach and finish after the restart",
            )
            assert "srun-exit=0" in out, out
            runs = node.read_file(started).count("started")
            assert runs == 1, f"the step started {runs} times: re-attaching must not relaunch it"
        finally:
            # A controller stuck draining ignores the SIGTERM the fixture teardown
            # sends, and a stray srun would retry into the next test's cluster.
            node.exec_allow_fail(
                f"kill -9 {pid} 2>/dev/null; pkill -9 -f '{cluster.bin_dir}/[s]run -J shutdown-hold'"
            )
