# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""A held job whose cancel never reached the node is stopped by the node's next
heartbeat, and a launch onto its GPUs is refused naming it until then."""

import time

import pytest

from cluster import block_agent_port, parse_job_id, wait_job_state

# One agent heartbeat is 30s.
HEARTBEAT_WINDOW = 45
# CancelJob gives up after 5s. Stay closed longer than that.
CANCEL_BLOCK = 8
# From the heartbeat that sends the cancel to the process being gone.
REAP_WINDOW = 15
# The heartbeat line is debug-only.
HEARTBEAT_LOG = "info,spurd::reporter=debug"
# Unusual durations, so the host-side lookup matches only these jobs.
RUNNING_SECS = 7311
HELD_SECS = 7312
HOLDER_SECS = 7313
CHALLENGER_SECS = 7314


def _start(cluster, config_overrides=None):
    cluster.agent_env["RUST_LOG"] = HEARTBEAT_LOG
    cluster.start(config_overrides)


def _sleep_script(cluster, name: str, secs: int) -> str:
    """The sleep duration tags the process; a job in its own PID namespace cannot report a host pid."""
    return cluster.write_file(f"{name}.sh", f"#!/bin/bash\nsleep {secs}\n", all_nodes=True)


def _pid_alive(node, pid: int) -> bool:
    out = node.exec(f"kill -0 {int(pid)} && echo alive || echo dead", check=False)
    return "alive" in out


def _kill_sleeps(node, *secs: int):
    """A held process this test failed to see stopped would match the next run."""
    for s in secs:
        node.exec(f"pkill -f '[s]leep {s}$' || true", check=False)


def _sleep_pid(cluster, node, secs: int, job_id: int) -> int:
    deadline = time.time() + 30
    while time.time() < deadline:
        out = node.exec(f"pgrep -f '[s]leep {secs}$' || true", check=False).split()
        if len(out) == 1 and out[0].isdigit():
            return int(out[0])
        if len(out) > 1:
            raise RuntimeError(f"more than one 'sleep {secs}' on the node: {out}")
        time.sleep(1)
    raise TimeoutError(
        f"no 'sleep {secs}' process on the node\n"
        + node.exec("ps -eo user,pid,ppid,args | grep -E '[s]leep|[s]purstepd' || true", check=False)
        + cluster.scontrol("show", "job", str(job_id))
        + "\n"
        + cluster.spurd_log(0)[-3000:]
    )


def _heartbeats(cluster) -> int:
    return cluster.spurd_log(0).count("heartbeat sent")


def _wait_for_heartbeat(cluster):
    seen = _heartbeats(cluster)
    deadline = time.time() + HEARTBEAT_WINDOW
    while time.time() < deadline:
        if _heartbeats(cluster) > seen:
            return
        time.sleep(1)
    raise TimeoutError("no agent heartbeat logged")


def _watch(node, pid: int, must_live: dict[str, int], seconds: int) -> bool:
    """Poll for the whole window. Whether *pid* died; every *must_live* pid must not."""
    died = False
    deadline = time.time() + seconds
    while time.time() < deadline:
        died = died or not _pid_alive(node, pid)
        for name, other in must_live.items():
            assert _pid_alive(node, other), f"the heartbeat stopped the {name} job"
        time.sleep(1)
    return died


class TestHeldJobStillRunning:
    def test_heartbeat_stops_only_a_held_job(self, unstarted_cluster):
        cluster = unstarted_cluster
        _start(cluster)
        node_name = cluster.node_names[0]
        node = cluster.nodes[0]
        running_id = None
        held_id = None
        try:
            running_script = _sleep_script(cluster, "running", RUNNING_SECS)
            held_script = _sleep_script(cluster, "held", HELD_SECS)
            running = cluster.sbatch(
                ["-J", "running", "-N", "1", f"--nodelist={node_name}", running_script]
            )
            held = cluster.sbatch(
                ["-J", "held", "-N", "1", f"--nodelist={node_name}", held_script]
            )
            running_id = parse_job_id(running)
            held_id = parse_job_id(held)
            assert running_id is not None, f"submit failed:\n{running}"
            assert held_id is not None, f"submit failed:\n{held}"
            wait_job_state(cluster, running_id, "R", timeout=60)
            wait_job_state(cluster, held_id, "R", timeout=60)

            running_pid = _sleep_pid(cluster, node, RUNNING_SECS, running_id)
            held_pid = _sleep_pid(cluster, node, HELD_SECS, held_id)

            # A closed agent port makes the requeuehold's cancel give up.
            with block_agent_port(cluster, 0):
                cluster.scontrol("requeuehold", str(held_id))
                time.sleep(CANCEL_BLOCK)
                held_show = cluster.scontrol("show", "job", str(held_id))
                assert "Reason=JobHeldUser" in held_show, held_show
                assert _pid_alive(node, held_pid), "held cancel landed while the agent port was closed"

            _wait_for_heartbeat(cluster)
            held_died = _watch(node, held_pid, {"running": running_pid}, REAP_WINDOW)
            assert held_died, (
                "heartbeat left the held job running:\n"
                + cluster.scontrol("show", "job", str(held_id))
            )
        finally:
            if running_id is not None:
                cluster.cli_allow_fail(["scancel", str(running_id)])
            if held_id is not None:
                cluster.cli_allow_fail(["scancel", str(held_id)])
            _kill_sleeps(node, RUNNING_SECS, HELD_SECS)

    @pytest.mark.gpu
    def test_refusal_names_the_holder_until_the_heartbeat_frees_it(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.gpu_preflight(1)
        _start(cluster)
        node_name = cluster.node_names[0]
        node = cluster.nodes[0]
        gpus = cluster.node_gpu_count(node_name)
        if gpus < 1:
            pytest.skip(f"{node_name} has no schedulable GPUs")
        holder_id = None
        challenger_id = None
        try:
            script = _sleep_script(cluster, "holder", HOLDER_SECS)
            challenger_script = _sleep_script(cluster, "challenger", CHALLENGER_SECS)
            # Every GPU on the node, so the second launch conflicts whichever it is given.
            gres = f"--gres=gpu:{gpus}"
            submitted = cluster.sbatch(
                ["-J", "holder", "-N", "1", f"--nodelist={node_name}", gres, script]
            )
            holder_id = parse_job_id(submitted)
            assert holder_id is not None, f"submit failed:\n{submitted}"
            wait_job_state(cluster, holder_id, "R", timeout=90)
            holder_pid = _sleep_pid(cluster, node, HOLDER_SECS, holder_id)

            # Starting right after a heartbeat leaves a full interval for the
            # second launch to land before the reclaim.
            _wait_for_heartbeat(cluster)
            with block_agent_port(cluster, 0):
                cluster.scontrol("requeuehold", str(holder_id))
                time.sleep(CANCEL_BLOCK)
                show = cluster.scontrol("show", "job", str(holder_id))
                assert "Reason=JobHeldUser" in show, show
                assert _pid_alive(node, holder_pid), "cancel landed while the agent port was closed"

            log_before = cluster.spurd_log(0)
            challenger = cluster.sbatch(
                ["-J", "challenger", "-N", "1", f"--nodelist={node_name}", gres, challenger_script]
            )
            challenger_id = parse_job_id(challenger)
            assert challenger_id is not None, f"submit failed:\n{challenger}"

            deadline = time.time() + 25
            fresh = ""
            while time.time() < deadline:
                fresh = cluster.spurd_log(0)[len(log_before):]
                if "rejecting dispatch" in fresh:
                    break
                time.sleep(1)
            assert "rejecting dispatch" in fresh, (
                "second launch was not refused\n"
                + cluster.scontrol("show", "job", str(challenger_id))
                + "\n"
                + fresh[-2000:]
            )
            assert f"gpu_owners=[{holder_id}]" in fresh, f"refusal did not name job {holder_id}:\n{fresh[-2000:]}"

            wait_job_state(cluster, challenger_id, "R", timeout=HEARTBEAT_WINDOW + 30)
            _sleep_pid(cluster, node, CHALLENGER_SECS, challenger_id)
            assert not _pid_alive(node, holder_pid), "challenger started while the holder still ran"
        finally:
            if holder_id is not None:
                cluster.cli_allow_fail(["scancel", str(holder_id)])
            if challenger_id is not None:
                cluster.cli_allow_fail(["scancel", str(challenger_id)])
            _kill_sleeps(node, HOLDER_SECS, CHALLENGER_SECS)
