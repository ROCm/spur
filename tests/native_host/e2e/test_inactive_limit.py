# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for InactiveLimit reaping of interactive allocations.

A live client must keep its allocation across a controller restart. Under the
native auth plugin every restart changes the credential epoch, so a client that
kept its submit-time credential would fail every later keepalive and have its
attended allocation reaped one limit later.

A client that goes silent is reaped like a time-limited job: the srun step gets
SIGTERM, the job sits in COMPLETING for up to timeout_kill_wait_secs, and it ends TIMEOUT.
The SIGTERM is final, so a client that pings again afterwards does not save it.
"""

import shlex
import threading
import time

import pytest

from cluster import _TERMINAL_STATE_CODES, job_state, wait_job, wait_job_state, wait_until

# The smallest limit the config accepts is two keepalive intervals (30 s each).
INACTIVE_LIMIT_SECS = 60
KEEPALIVE_INTERVAL_SECS = 30
TIMEOUT_KILL_WAIT_SECS = 40
RUNNING_BOUND = 30
# The last ping can be up to one interval old, and the reaper runs on a tick.
SIGNAL_BOUND = INACTIVE_LIMIT_SECS + 2 * KEEPALIVE_INTERVAL_SECS
FINISH_BOUND = TIMEOUT_KILL_WAIT_SECS + 40


class TestInactiveLimit:
    def test_a_live_salloc_survives_a_controller_restart_under_native_auth(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        paths = cluster.install_native_jwks()
        sock = f"{cluster.remote_dir}/auth.sock"
        cluster.cli_env = {
            "SPUR_AUTH_PLUGIN": "spur",
            "SPUR_AUTH_SOCKET": sock,
            "SPUR_CLUSTER_NAME": "e2e-test",
        }
        cluster.daemon_env = {
            "SPUR_AUTH_JWKS": paths["auth"],
            "SPUR_CRED_VERIFICATION_JWKS": paths["cred-verification"],
            "SPUR_CONTROLLER_VERIFICATION_JWKS": paths["controller-verification"],
            "SPUR_AUTH_SOCKET": sock,
            "SPUR_CLUSTER_NAME": "e2e-test",
        }
        cluster.controller_env = {
            "SPUR_CRED_SIGNING_JWKS": paths["cred-signing"],
            "SPUR_CONTROLLER_SIGNING_JWKS": paths["controller-signing"],
            "SPUR_NODE_SIGNING_JWKS": paths["node-signing"],
        }
        cluster.start_native_mint(paths["auth"], sock)
        cluster.start(
            config_overrides={
                "auth": {"plugin": "spur", "mode": "required"},
                "scheduler": {"inactive_limit_secs": INACTIVE_LIMIT_SECS},
            },
        )

        job_name = f"inactive-live-{time.time_ns()}"
        result: dict[str, object] = {}

        def run():
            # The shell outlives the restart plus two full limits.
            result["code"], result["out"] = cluster.salloc_run(
                "sleep 200\n",
                salloc_args=["-N", "1", "-t", "10:00", "-J", job_name],
            )

        thread = threading.Thread(target=run)
        thread.start()
        try:
            deadline = time.time() + RUNNING_BOUND
            job_id = None
            while time.time() < deadline:
                ids = cluster.running_job_ids_by_name(job_name)
                if ids:
                    job_id = ids[0]
                    break
                time.sleep(1)
            assert job_id, "the salloc allocation never reached RUNNING"

            cluster.restart_controller()
            # A client still on its pre-restart credential would have been
            # signalled within one limit plus the reaper's tick.
            time.sleep(2 * INACTIVE_LIMIT_SECS + 10)
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobState=RUNNING" in show, (
                f"an attended salloc was reaped after a controller restart:\n{show}"
            )
        finally:
            thread.join(timeout=300)

        assert result.get("code") == 0, result.get("out")


class _SilentSalloc:
    """A salloc client whose srun step traps SIGTERM, and that can be silenced.

    SIGSTOP freezes only the salloc process, which owns the keepalive pings; the
    allocation shell and its step keep running, so the job is busy but unattended.
    """

    def __init__(self, cluster):
        self.cluster = cluster
        self.job_name = f"inactive-silent-{time.time_ns()}"
        self.step_marker = f"{cluster.remote_dir}/{self.job_name}.term"
        self.started_marker = f"{cluster.remote_dir}/{self.job_name}.started"
        self.result: dict[str, object] = {}
        self.job_id = None
        self.thread = None

    def _pattern(self) -> str:
        return shlex.quote(f"^[^ ]*/spur salloc .*-J {self.job_name}( |$)")

    def start(self) -> int:
        cluster = self.cluster
        step = cluster.write_file(
            f"{self.job_name}-step.sh",
            "#!/bin/bash\n"
            f"trap 'echo TERM >> {self.step_marker}' TERM\n"
            f"echo started > {self.started_marker}\n"
            "while true; do sleep 1; done\n",
        )
        node = cluster.node_names[0]

        def run():
            self.result["code"], self.result["out"] = cluster.salloc_run(
                f"srun {step}\n",
                salloc_args=["-N", "1", "-w", node, "-t", "10:00", "-J", self.job_name],
            )

        self.thread = threading.Thread(target=run)
        self.thread.start()
        self.job_id = wait_until(
            lambda: (cluster.running_job_ids_by_name(self.job_name) or [None])[0],
            timeout_s=RUNNING_BOUND, interval_s=1,
            desc="the salloc allocation to reach RUNNING",
        )
        wait_until(
            lambda: "started" in cluster.read_output_on_any_node(self.started_marker),
            timeout_s=RUNNING_BOUND, interval_s=1,
            desc="the srun step to start",
        )
        return self.job_id

    def silence(self):
        self.cluster.nodes[0].exec(f"pkill -STOP -f {self._pattern()}")

    def resume(self):
        self.cluster.nodes[0].exec(f"pkill -CONT -f {self._pattern()}")

    def terminated(self) -> bool:
        return "TERM" in self.cluster.read_output_on_any_node(self.step_marker)

    def close(self):
        node = self.cluster.nodes[0]
        node.exec_allow_fail(f"pkill -CONT -f {self._pattern()}")
        node.exec_allow_fail(f"pkill -9 -f {self._pattern()}")
        if self.job_id is not None:
            self.cluster.cli_allow_fail(["scancel", str(self.job_id)])
        if self.thread is not None:
            self.thread.join(timeout=120)


def _states_until_terminal(cluster, job_id: int, timeout: int) -> list[str]:
    """Every distinct squeue state the job passes through until it is terminal."""
    seen: list[str] = []

    def terminal():
        state = job_state(cluster.squeue_all(), job_id)
        if state and (not seen or seen[-1] != state):
            seen.append(state)
        return state in _TERMINAL_STATE_CODES

    wait_until(terminal, timeout_s=timeout, interval_s=2, desc=f"job {job_id} to end")
    return seen


class TestInactiveLimitKillWait:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "auth": {"allow_root_jobs": True},
            "scheduler": {
                "inactive_limit_secs": INACTIVE_LIMIT_SECS,
                "timeout_kill_wait_secs": TIMEOUT_KILL_WAIT_SECS,
            },
        }

    def test_inactive_limit_warns_then_times_out(self, cluster):
        client = _SilentSalloc(cluster)
        try:
            job_id = client.start()
            client.silence()

            wait_until(client.terminated, timeout_s=SIGNAL_BOUND, interval_s=2,
                       desc="the step to be sent SIGTERM after its client went silent")
            # The step ignores SIGTERM, so the job waits out the kill wait in COMPLETING.
            wait_job_state(cluster, job_id, "CG", timeout=RUNNING_BOUND)

            assert wait_job(cluster, job_id, timeout=FINISH_BOUND) == "TO", (
                f"a reaped allocation ends TIMEOUT:\n{cluster.debug_job(job_id)}"
            )
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobState=TIMEOUT" in show, show
        finally:
            client.close()

    def test_inactive_limit_reconnect_does_not_save_the_job(self, cluster):
        client = _SilentSalloc(cluster)
        try:
            job_id = client.start()
            client.silence()

            wait_until(client.terminated, timeout_s=SIGNAL_BOUND, interval_s=2,
                       desc="the step to be sent SIGTERM after its client went silent")
            wait_job_state(cluster, job_id, "CG", timeout=RUNNING_BOUND)

            # The client resumes and pings inside the kill wait; the SIGTERM is final.
            client.resume()

            states = _states_until_terminal(cluster, job_id, FINISH_BOUND)
            assert states[-1] == "TO", (
                f"a client that pings again must not save the job; states {states}:\n"
                f"{cluster.debug_job(job_id)}"
            )
            assert "R" not in states, (
                f"the job must not return to RUNNING after the SIGTERM; states {states}"
            )
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobState=TIMEOUT" in show, show
        finally:
            client.close()
