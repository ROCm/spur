# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E regression coverage for the wedged-stepd force-reclaim path.

A cancelled job's local allocation only ever released via `stepd_liveness`
(pid + /proc start-ticks) — a supervisor that's alive but wedged (frozen,
deadlocked, stuck in a hung hook) reads identically to a healthy one, so
nothing gated on that check ever fences it. The node's resources then stay
marked busy forever, rejecting every job sent there until a human manually
intervenes.

This reproduces the wedge deterministically with `kill -STOP` on a real
`spurstepd` supervisor (real uncontrived "alive, not making progress"),
instead of relying on an actual deadlock, and asserts the node is usable
again within the force-reclaim window instead of never.
"""

import re
import threading
import time

import pytest

from cluster import (
    block_agent_port, job_state, parse_job_id, wait_job_state, wait_until,
)


def _supervisor_pids(cluster, node_index: int = 0) -> set[str]:
    node = cluster.nodes[node_index]
    return set(node.exec_allow_fail("pgrep -x spurstepd || true").split())


# Comfortably above the 30s force-reclaim window plus scheduling overhead
# observed in practice, well below the multi-minute stall the bug caused.
PROBE_RUNNING_BOUND = 120


class TestWedgedStepdReclaim:
    def test_a_cancelled_jobs_ledger_reclaims_even_if_its_supervisor_is_frozen(
        self, cluster
    ):
        script = cluster.write_file(
            "wedge-victim.sh", "#!/bin/bash\nsleep 120\n"
        )
        node = cluster.node_names[0]
        submitted_jobs = []
        frozen_pid = None
        try:
            job_id = parse_job_id(
                cluster.sbatch(["-J", "wedge-victim", "-w", node, "--exclusive", script])
            )
            assert job_id is not None
            submitted_jobs.append(job_id)
            wait_job_state(cluster, job_id, "R")

            pids = _supervisor_pids(cluster)
            assert pids, "a running job must have a supervisor"
            frozen_pid = next(iter(pids))
            cluster.nodes[0].exec(f"sudo kill -STOP {frozen_pid}")

            cluster.scancel(job_id)

            probe_script = cluster.write_file("wedge-probe.sh", "#!/bin/bash\nsleep 5\n")
            probe_job = parse_job_id(
                cluster.sbatch(["-J", "wedge-probe", "-w", node, "--exclusive", probe_script])
            )
            assert probe_job is not None
            submitted_jobs.append(probe_job)

            wait_job_state(cluster, probe_job, "R", timeout=PROBE_RUNNING_BOUND)
        finally:
            if frozen_pid is not None:
                # CONT before kill -9: a still-stopped process ignores
                # SIGKILL's delivery until it's scheduled again.
                cluster.nodes[0].exec_allow_fail(f"sudo kill -CONT {frozen_pid}")
                cluster.nodes[0].exec_allow_fail(f"sudo kill -9 {frozen_pid}")
            for job_id in submitted_jobs:
                cluster.scancel(job_id)

    def test_a_supervisor_that_recovers_before_the_hard_deadline_is_not_force_killed(
        self, cluster
    ):
        script = cluster.write_file(
            "wedge-recovers.sh", "#!/bin/bash\nsleep 120\n"
        )
        node = cluster.node_names[0]
        submitted_jobs = []
        frozen_pid = None
        try:
            job_id = parse_job_id(
                cluster.sbatch(["-J", "wedge-recovers", "-w", node, "--exclusive", script])
            )
            assert job_id is not None
            submitted_jobs.append(job_id)
            wait_job_state(cluster, job_id, "R")

            pids = _supervisor_pids(cluster)
            assert pids, "a running job must have a supervisor"
            frozen_pid = next(iter(pids))
            cluster.nodes[0].exec(f"sudo kill -STOP {frozen_pid}")
            cluster.scancel(job_id)

            # Resume well inside the force-reclaim window: the already-
            # delivered cancel gets to run once the process is scheduled
            # again, so this should resolve via the ordinary exit path.
            time.sleep(3)
            cluster.nodes[0].exec_allow_fail(f"sudo kill -CONT {frozen_pid}")
            frozen_pid = None

            probe_script = cluster.write_file(
                "wedge-recovers-probe.sh", "#!/bin/bash\nsleep 5\n"
            )
            probe_job = parse_job_id(
                cluster.sbatch(["-J", "wedge-recovers-probe", "-w", node, "--exclusive", probe_script])
            )
            assert probe_job is not None
            submitted_jobs.append(probe_job)

            # Bound well under the force-reclaim window: a supervisor that
            # was really just resumed, not genuinely wedged, must not need
            # to wait out that whole window to release.
            wait_job_state(cluster, probe_job, "R", timeout=15)
        finally:
            if frozen_pid is not None:
                cluster.nodes[0].exec_allow_fail(f"sudo kill -CONT {frozen_pid}")
                cluster.nodes[0].exec_allow_fail(f"sudo kill -9 {frozen_pid}")
            for job_id in submitted_jobs:
                cluster.scancel(job_id)


class TestWedgedInteractiveStepdReclaim:
    """A standalone `srun --pty` job's only stepd is a terminal placeholder,
    excluded from the old `owns_job_lifetime`-based fencing gate. Same wedge,
    same force-reclaim path, but on the job shape that gate used to skip.
    """

    def test_a_cancelled_interactive_only_jobs_ledger_reclaims_even_if_its_supervisor_is_frozen(
        self, cluster
    ):
        node = cluster.node_names[0]
        job_name = f"wedge-interactive-{time.time_ns()}"
        result: dict[str, object] = {}

        def run():
            result["code"], result["out"] = cluster.srun_with_exit([
                "-N", "1", "-w", node, "-t", "0:02", "-J", job_name, "--pty",
                "bash", "-c", "sleep 120",
            ])

        thread = threading.Thread(target=run)
        thread.start()
        submitted_jobs: list[int] = []
        frozen_pid = None
        try:
            deadline = time.time() + 30
            job_id = None
            while time.time() < deadline:
                ids = cluster.running_job_ids_by_name(job_name)
                if ids:
                    job_id = ids[0]
                    break
                time.sleep(1)
            assert job_id, "expected the standalone --pty job to appear"
            submitted_jobs.append(job_id)

            pids = _supervisor_pids(cluster)
            assert pids, "a running interactive job must have a supervisor"
            frozen_pid = next(iter(pids))
            cluster.nodes[0].exec(f"sudo kill -STOP {frozen_pid}")

            cluster.scancel(job_id)

            probe_script = cluster.write_file(
                "wedge-interactive-probe.sh", "#!/bin/bash\nsleep 5\n"
            )
            probe_job = parse_job_id(
                cluster.sbatch(["-J", "wedge-interactive-probe", "-w", node, "--exclusive", probe_script])
            )
            assert probe_job is not None
            submitted_jobs.append(probe_job)

            wait_job_state(cluster, probe_job, "R", timeout=PROBE_RUNNING_BOUND)
        finally:
            if frozen_pid is not None:
                cluster.nodes[0].exec_allow_fail(f"sudo kill -CONT {frozen_pid}")
                cluster.nodes[0].exec_allow_fail(f"sudo kill -9 {frozen_pid}")
            for job_id in submitted_jobs:
                cluster.scancel(job_id)
            thread.join(timeout=60)


# The controller's dial budget to the unreachable node. A step's launch there fails exactly
# this long after it starts, which is what delays the step cancel. It dwarfs the few seconds
# the test needs to freeze the supervisor, so the ordering holds with room to spare.
_DIAL_BUDGET_SECS = 60
# The agent SIGKILLs a cancelled step this long after the cancel (STEP_CANCEL_ESCALATION).
_STEP_ESCALATION_SECS = 5
# Time for the agent to have recorded the step's pid with its supervisor before that is frozen.
_PID_RECORDED_SETTLE_SECS = 3
# Polling and scheduling headroom on top of the escalation.
_STEP_CANCEL_SLACK_SECS = 20
_STEP_SLEEP_SECS = 617


def _step_sleep_pids(node) -> list[str]:
    """Host pids of the step's `sleep` (found by its unique duration), which also sees
    processes inside a job's pid namespace."""
    return node.exec_allow_fail(
        "for p in $(pgrep -x sleep); do "
        "tr '\\0' ' ' 2>/dev/null < /proc/$p/cmdline "
        f"| grep -qE 'sleep {_STEP_SLEEP_SECS}( |$)' && echo $p; done"
    ).split()


def _cancel_steps_sent(cluster, job_id: int) -> int:
    return len(
        re.findall(
            rf"sent CancelStep.*job_id={job_id}(?!\d)|job_id={job_id}(?!\d).*sent CancelStep",
            cluster.spurctld_log(),
        )
    )


class TestStepCancelWithAFrozenSupervisor:
    """A cancelled step must die even when its supervisor cannot be reached.

    The cancel is made to arrive late and on purpose: one node of a two-node srun is
    unreachable from the controller, so its launch fails only once the controller's dial
    budget runs out and the step on the other node is cancelled then. That leaves the
    test the whole budget to freeze the surviving step's supervisor first, so the
    ordering does not depend on a race. The step ignores SIGTERM, so only the agent's own
    SIGKILL after the escalation delay can end it.
    """

    @pytest.fixture(params=[False, True], ids=["user-spurd", "root-spurd"])
    def agent_as_root(self, request):
        return request.param

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "auth": {"allow_root_jobs": True},
            "controller": {"agent_connect_timeout_secs": _DIAL_BUDGET_SECS},
        }

    def test_the_agent_kills_the_surviving_step_without_its_supervisor(self, cluster):
        if len(cluster.nodes) < 2:
            pytest.skip("needs two nodes: one to run the step, one to fail its launch")
        node = cluster.nodes[0]
        go = f"{cluster.remote_dir}/step-cancel.go"
        step = cluster.write_file(
            "step-cancel-step.sh",
            f"#!/bin/bash\ntrap '' TERM\nsleep {_STEP_SLEEP_SECS}\n",
            all_nodes=True,
        )
        batch = cluster.write_file(
            "step-cancel.sh",
            "#!/bin/bash\n"
            f"while [ ! -e '{go}' ]; do sleep 1; done\n"
            f"srun -N2 '{step}'\n"
            "sleep 600\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "step-cancel", "-N", "2", "-w", ",".join(cluster.node_names[:2]), batch]
            )
        )
        assert job_id is not None, "sbatch failed"
        frozen: set[str] = set()
        try:
            wait_job_state(cluster, job_id, "R", timeout=60)
            job_supervisors = _supervisor_pids(cluster)
            with block_agent_port(cluster, node_index=1):
                node.exec(f"touch '{go}'")
                started = time.time()
                wait_until(
                    lambda: _step_sleep_pids(node),
                    timeout_s=30, interval_s=1,
                    desc="the srun step to start on node 0",
                )
                time.sleep(_PID_RECORDED_SETTLE_SECS)
                frozen = _supervisor_pids(cluster) - job_supervisors
                assert frozen, "the step must have a supervisor of its own"
                for pid in frozen:
                    node.exec(f"sudo kill -STOP {pid}")
                assert _cancel_steps_sent(cluster, job_id) == 0, (
                    "the step was cancelled before its supervisor was frozen, which "
                    "would prove nothing"
                )

                budget = _DIAL_BUDGET_SECS + _STEP_ESCALATION_SECS + _STEP_CANCEL_SLACK_SECS
                wait_until(
                    lambda: not _step_sleep_pids(node),
                    timeout_s=max(1, int(started + budget - time.time())), interval_s=1,
                    desc="the cancelled step to be SIGKILLed with its supervisor frozen",
                )
            assert _cancel_steps_sent(cluster, job_id) > 0, (
                "the step ended, but not by the controller's cancel"
            )
            assert job_state(cluster.squeue_all(), job_id) == "R", (
                "a step cancel must keep the job's allocation"
            )
        finally:
            for pid in frozen:
                node.exec_allow_fail(f"sudo kill -CONT {pid}")
                node.exec_allow_fail(f"sudo kill -9 {pid}")
            cluster.scancel(job_id)
