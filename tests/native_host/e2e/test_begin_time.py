# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for the reason reported while --begin defers a job.

A job held back by ``--begin`` sits in PENDING until its start time arrives.
Reporting ``None`` for that wait is indistinguishable from a job the scheduler
has simply not reached yet, so there is no way to tell a deliberate deferral
apart from an idle scheduler pass. Slurm reports ``BeginTime`` here.

The reason is recomputed each scheduling cycle, so both assertions below poll
rather than sampling once.
"""

import time

from cluster import parse_job_id, wait_job, wait_job_state

DEFER_SECONDS = 40


def _wait_for_reason(cluster, job_id: int, reason: str, timeout: int = 60) -> str:
    """Poll ``scontrol show job`` until it reports *reason*. Returns the output."""
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        last = cluster.scontrol("show", "job", str(job_id))
        if f"Reason={reason}" in last:
            return last
        time.sleep(2)
    raise AssertionError(
        f"job {job_id} never reported Reason={reason} within {timeout}s; "
        f"last scontrol output:\n{last}"
    )


class TestBeginTimeReason:
    def test_deferred_job_reports_begin_time_then_runs(self, cluster):
        script = cluster.write_file("begin-ok.sh", "#!/bin/bash\necho ran\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "begin-defer", f"--begin=now+{DEFER_SECONDS}seconds", script])
        )
        assert job_id is not None

        wait_job_state(cluster, job_id, "PD")
        show = _wait_for_reason(cluster, job_id, "BeginTime")
        assert "JobState=PENDING" in show, show

        # squeue's REASON column is the surface most users actually read.
        squeue = cluster.squeue([])
        assert "(BeginTime)" in squeue, squeue

        # The deferral itself must still work: the reason is cosmetic, and a job
        # stuck reporting BeginTime forever would be a worse bug than None.
        state = wait_job(cluster, job_id, timeout=DEFER_SECONDS + 120)
        assert state == "CD", f"deferred job must still run to completion, got {state}"
        assert "Reason=BeginTime" not in cluster.scontrol("show", "job", str(job_id))

    def test_job_without_a_begin_time_never_reports_begin_time(self, cluster):
        # Guards against tagging every job the scheduler passes over.
        script = cluster.write_file("begin-plain.sh", "#!/bin/bash\necho ran\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "begin-plain", script]))
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=120)
        assert state == "CD", f"expected COMPLETED, got {state}"
        assert "Reason=BeginTime" not in cluster.scontrol("show", "job", str(job_id))
