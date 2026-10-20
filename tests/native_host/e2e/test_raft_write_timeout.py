# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: a write the Raft leader cannot commit fails in bounded time.

With both followers frozen the leader can append entries but never commit
them. A requeue issued then must fail with "outcome unknown" once the propose
timeout passes instead of hanging, and after the followers resume the job must
settle with exactly one live copy and the scheduler must still place new work.
"""

import time

from cluster import CONTROLLER_PORT, job_state, parse_job_id, wait_job_state

PROPOSE_TIMEOUT_SECS = 30
# A CLI still waiting at this point would have hung without the bound.
CLI_DEADLINE_SECS = 90
# Unique duration so the pid lookup matches only this test's job.
SLEEP_SECS = 1871
SETTLE_SECS = 180


def _sleep_pids(cluster) -> list[str]:
    """Pids of this job's `sleep` across every node; exact-name match avoids
    self-matching the shell that runs the check."""
    cmd = (
        "for p in $(pgrep -x sleep); do "
        "tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null "
        f"| grep -qE 'sleep {SLEEP_SECS}( |$)' && echo $p; done"
    )
    pids = []
    for node in cluster.nodes:
        pids += [f"{node.host}:{p}" for p in node.exec_allow_fail(cmd).split()]
    return pids


def _requeue_against(cluster, leader: int, job_id: int) -> tuple[int, str, float]:
    addr = f"http://{cluster.nodes[leader].host}:{CONTROLLER_PORT}"
    cmd = cluster._cli_command(["scontrol", "requeue", str(job_id)], controller_addr=addr)
    start = time.time()
    code, out = cluster.nodes[0].exec_with_exit(f"timeout {CLI_DEADLINE_SECS} env {cmd}")
    return code, out, time.time() - start


def _wait_one_live_copy(cluster, job_id: int) -> tuple[str | None, list[str]]:
    deadline = time.time() + SETTLE_SECS
    state, pids = None, []
    while time.time() < deadline:
        state = job_state(cluster.squeue_all(), job_id)
        pids = _sleep_pids(cluster)
        if state == "R" and len(pids) == 1:
            return state, pids
        time.sleep(3)
    return state, pids


class TestRaftWriteTimeout:
    def test_uncommittable_requeue_fails_in_bounded_time(self, ha_cluster):
        cluster = ha_cluster
        n = cluster.ha_controller_count
        job_id = probe_id = None
        try:
            script = cluster.write_file(
                "raft-timeout.sh", f"#!/bin/bash\nsleep {SLEEP_SECS}\n"
            )
            job_id = parse_job_id(cluster.sbatch(["-J", "raft-timeout", "-N", "1", script]))
            assert job_id is not None, "sbatch failed"
            wait_job_state(cluster, job_id, "R", timeout=60)
            assert len(_sleep_pids(cluster)) == 1

            cluster._wait_leader_elected()
            leader = cluster._current_raft_leader()
            assert leader is not None, "no stable leader"
            followers = [i for i in range(n) if i != leader]
            try:
                for i in followers:
                    cluster.signal_controller(i, "STOP")
                code, out, elapsed = _requeue_against(cluster, leader, job_id)
            finally:
                for i in range(n):
                    cluster.signal_controller(i, "CONT")

            assert code not in (0, 124), f"exit {code} after {elapsed:.0f}s:\n{out}"
            assert "outcome unknown" in out, out
            assert PROPOSE_TIMEOUT_SECS - 5 <= elapsed < CLI_DEADLINE_SECS - 10, elapsed

            cluster._wait_leader_elected(timeout=120)
            state, pids = _wait_one_live_copy(cluster, job_id)
            assert state == "R" and len(pids) == 1, (
                f"job {job_id} did not settle to one running copy: state={state} pids={pids}"
            )

            probe = cluster.write_file("raft-timeout-probe.sh", "#!/bin/bash\nsleep 30\n")
            probe_id = parse_job_id(cluster.sbatch(["-J", "raft-probe", "-N", "1", probe]))
            assert probe_id is not None, "probe sbatch failed"
            wait_job_state(cluster, probe_id, "R", timeout=90)
        finally:
            for jid in (job_id, probe_id):
                if jid is not None:
                    cluster.cli_allow_fail(["scancel", str(jid)])
