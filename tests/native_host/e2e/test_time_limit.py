# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for wall-time expiry attribution.

The time-limit watchdog terminates an over-running job in two phases: SIGTERM
once the deadline passes, then SIGKILL after a grace period. Both outcomes are
a wall-time expiry and must report TIMEOUT, but the two paths finalize the job
through different code: a job that dies on the SIGTERM is finalized from its
agent's completion report, while one that ignores SIGTERM is finalized by the
watchdog itself. Only the second used to report TIMEOUT — the well-behaved job
reported FAILED, indistinguishable from a crash.

Each test submits with a short ``--time`` and waits out the watchdog, so the
timeouts below allow for the 10s watchdog tick plus the 30s grace period.
"""

from cluster import parse_job_id, wait_job

TIME_LIMIT = "00:00:20"
# Deadline + watchdog tick + grace period + SIGKILL reap, with slack.
FINISH_TIMEOUT = 180


def _show_job(cluster, job_id: int) -> str:
    return cluster.scontrol("show", "job", str(job_id))


class TestTimeLimitExpiry:
    def test_job_exiting_on_sigterm_reports_timeout(self, cluster):
        script = cluster.write_file(
            "timelimit-plain.sh",
            "#!/bin/bash\nsleep 300\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "tl-plain", f"--time={TIME_LIMIT}", script]))
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=FINISH_TIMEOUT)
        assert state == "TO", (
            f"a job killed by its wall-time limit must report TIMEOUT, got {state}. "
            f"FAILED here means the completion report was read as an ordinary "
            f"signal death:\n{cluster.debug_job(job_id)}"
        )

        show = _show_job(cluster, job_id)
        assert "JobState=TIMEOUT" in show, show
        assert "Reason=TimeLimit" in show, show
        # The agent's real exit status survives the timeout verdict rather than
        # being replaced by the watchdog's synthetic -1. Its exact value depends
        # on whether the shell dies from SIGTERM or exits 128+15, so only the
        # synthetic value is ruled out here.
        assert "ExitCode=-1:" not in show, show

    def test_job_ignoring_sigterm_reports_timeout(self, cluster):
        script = cluster.write_file(
            "timelimit-trap.sh",
            '#!/bin/bash\ntrap "" TERM\nsleep 300\n',
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "tl-trap", f"--time={TIME_LIMIT}", script]))
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=FINISH_TIMEOUT)
        assert state == "TO", (
            f"a job that outlives the grace period must report TIMEOUT, got "
            f"{state}:\n{cluster.debug_job(job_id)}"
        )

        show = _show_job(cluster, job_id)
        assert "JobState=TIMEOUT" in show, show
        assert "Reason=TimeLimit" in show, show

    def test_job_finishing_inside_its_limit_is_untouched(self, cluster):
        # Guards against over-applying the timeout verdict: a job that ends on
        # its own must keep reporting its real outcome.
        script = cluster.write_file(
            "timelimit-fast-fail.sh",
            "#!/bin/bash\nexit 3\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "tl-fast", f"--time={TIME_LIMIT}", script]))
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=FINISH_TIMEOUT)
        assert state == "F", f"expected FAILED for a genuine non-zero exit, got {state}"

        show = _show_job(cluster, job_id)
        assert "JobState=FAILED" in show, show
        assert "ExitCode=3:0" in show, show
        assert "TimeLimit" not in show, show
