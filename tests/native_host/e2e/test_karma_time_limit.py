# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for karma score prerequisite: time_limit_secs is recorded in
the accounting DB and returned by sacct.

Requires Postgres on node 0 (the accounting_cluster fixture).
"""

import time

from cluster import parse_job_id, wait_job, wait_sacct_row


class TestTimeLimitAccounting:
    """Verify that the wall-time limit a user requests is persisted in the
    accounting database and surfaced by sacct --format=TimeLimit."""

    def test_timed_job_records_time_limit(self, accounting_cluster):
        """A job submitted with --time=5 must show 00:05:00 in sacct."""
        c = accounting_cluster
        script = c.write_file(
            "karma-timed.sh", "#!/bin/bash\nsleep 2\n"
        )
        job_id = parse_job_id(
            c.sbatch(["-J", "karma-timed", "-N", "1", "--time=5", script])
        )
        assert job_id is not None
        wait_job(c, job_id, timeout=60)
        row = wait_sacct_row(c, job_id, "JobID,TimeLimit,Elapsed,State")
        fields = row.split()
        assert fields[1] == "00:05:00", (
            f"expected TimeLimit 00:05:00, got {fields[1]!r} (row: {row!r})"
        )
        assert fields[3] == "COMPLETED", (
            f"expected COMPLETED, got {fields[3]!r}"
        )

    def test_timed_job_timeout_records_limit(self, accounting_cluster):
        """A job that hits its wall time limit must still show the original
        requested limit in sacct, not the elapsed time."""
        c = accounting_cluster
        script = c.write_file(
            "karma-timeout.sh", "#!/bin/bash\nsleep 300\n"
        )
        job_id = parse_job_id(
            c.sbatch(["-J", "karma-timeout", "-N", "1", "--time=1", script])
        )
        assert job_id is not None
        wait_job(c, job_id, timeout=120)
        row = wait_sacct_row(c, job_id, "JobID,TimeLimit,State")
        fields = row.split()
        assert fields[1] == "00:01:00", (
            f"expected TimeLimit 00:01:00, got {fields[1]!r}"
        )
        assert fields[2] == "TIMEOUT", (
            f"expected TIMEOUT state, got {fields[2]!r}"
        )

    def test_time_limit_survives_in_postgres(self, accounting_cluster):
        """Verify the raw time_limit_secs column in PostgreSQL has the correct
        integer value (seconds), not just the formatted sacct output."""
        c = accounting_cluster
        script = c.write_file(
            "karma-pg.sh", "#!/bin/bash\nsleep 2\n"
        )
        job_id = parse_job_id(
            c.sbatch(["-J", "karma-pg", "-N", "1", "--time=10", script])
        )
        assert job_id is not None
        wait_job(c, job_id, timeout=60)
        time.sleep(5)
        out = c.psql(
            f"SELECT time_limit_secs FROM jobs WHERE job_id = {job_id}"
        )
        assert out.strip() == "600", (
            f"expected time_limit_secs=600 in postgres, got {out.strip()!r}"
        )

    def test_sub_minute_limit_preserved(self, accounting_cluster):
        """--time=0:00:30 must store 30 seconds, not truncate to 0."""
        c = accounting_cluster
        script = c.write_file(
            "karma-sub-min.sh", "#!/bin/bash\nsleep 2\n"
        )
        job_id = parse_job_id(
            c.sbatch(
                ["-J", "karma-sub-min", "-N", "1", "--time=0:00:30", script]
            )
        )
        assert job_id is not None
        wait_job(c, job_id, timeout=60)
        time.sleep(5)
        out = c.psql(
            f"SELECT time_limit_secs FROM jobs WHERE job_id = {job_id}"
        )
        assert out.strip() == "30", (
            f"expected time_limit_secs=30 for --time=0:00:30, got {out.strip()!r}"
        )
