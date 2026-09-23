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

import threading
import time

from cluster import parse_job_id, wait_job_state


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
