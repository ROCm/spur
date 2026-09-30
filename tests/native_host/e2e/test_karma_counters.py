# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for native karma Prometheus counters on /metrics/jobs-users-accts.

Submits jobs covering completed, failed, cancelled, timeout scenarios
and verifies all 10 counters at the metrics endpoint.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job


KARMA_COUNTERS = [
    "spur_user_jobs_submitted_total",
    "spur_user_jobs_completed_total",
    "spur_user_jobs_failed_total",
    "spur_user_jobs_timeout_total",
    "spur_user_jobs_node_fail_total",
    "spur_user_jobs_cancelled_total",
    "spur_user_gpus_requested_total",
    "spur_user_overflow_borrows_total",
    "spur_user_walltime_requested_seconds_total",
    "spur_user_walltime_actual_seconds_total",
]


def _scrape_counters(cluster, username):
    """Curl /metrics/jobs-users-accts via SSH and parse counter values."""
    body = cluster.nodes[0].exec(
        "curl -sf http://127.0.0.1:6822/metrics/jobs-users-accts"
    )
    result = {}
    for line in body.splitlines():
        if line.startswith("#"):
            continue
        for counter in KARMA_COUNTERS:
            pattern = (
                rf'^{re.escape(counter)}\{{username='
                rf'"{re.escape(username)}"\}}\s+(\S+)'
            )
            m = re.match(pattern, line)
            if m:
                result[counter] = float(m.group(1))
    return result


def _scrape_body(cluster):
    """Return the raw metrics body via SSH."""
    return cluster.nodes[0].exec(
        "curl -sf http://127.0.0.1:6822/metrics/jobs-users-accts"
    )


class TestKarmaCounters:
    """Verify that native karma counters increment correctly."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"metrics": {"high_cardinality": True}}

    @pytest.fixture(autouse=True)
    def setup(self, cluster):
        self.c = cluster
        self.username = self.c.nodes[0].user or "root"

    def _submit_and_wait(self, args, timeout=60):
        job_id = parse_job_id(self.c.sbatch(args))
        assert job_id is not None
        wait_job(self.c, job_id, timeout=timeout)
        time.sleep(2)
        return job_id

    def test_completed_job_increments_counters_and_type_is_counter(self):
        """A successful job increments submitted, completed,
        walltime_requested, walltime_actual, and all TYPE lines
        are declared as counter (not gauge)."""
        script = self.c.write_file(
            "karma-ok.sh", "#!/bin/bash\nsleep 3\n"
        )
        self._submit_and_wait(
            ["-J", "karma-ok", "-N", "1", "--time=5", script]
        )

        counters = _scrape_counters(self.c, self.username)
        assert counters.get("spur_user_jobs_submitted_total", 0) >= 1
        assert counters.get("spur_user_jobs_completed_total", 0) >= 1
        assert counters.get(
            "spur_user_walltime_requested_seconds_total", 0
        ) >= 300
        assert counters.get(
            "spur_user_walltime_actual_seconds_total", 0
        ) >= 3

        body = _scrape_body(self.c)
        for counter in KARMA_COUNTERS:
            base = counter.removesuffix("_total")
            assert f"# TYPE {base} counter" in body, (
                f"{base} is not declared as counter type"
            )

    def test_failed_job_increments_failed_counter(self):
        """A job that exits non-zero increments failed_total."""
        before = _scrape_counters(self.c, self.username)
        failed_before = before.get("spur_user_jobs_failed_total", 0)

        script = self.c.write_file(
            "karma-fail.sh", "#!/bin/bash\nexit 1\n"
        )
        self._submit_and_wait(
            ["-J", "karma-fail", "-N", "1", "--time=3", script]
        )

        after = _scrape_counters(self.c, self.username)
        assert after.get("spur_user_jobs_failed_total", 0) > failed_before

    def test_cancelled_job_increments_cancelled_counter(self):
        """A cancelled running job increments cancelled_total."""
        before = _scrape_counters(self.c, self.username)
        cancelled_before = before.get(
            "spur_user_jobs_cancelled_total", 0
        )

        script = self.c.write_file(
            "karma-cancel.sh", "#!/bin/bash\nsleep 600\n"
        )
        job_id = parse_job_id(
            self.c.sbatch(
                ["-J", "karma-cancel", "-N", "1", "--time=10", script]
            )
        )
        assert job_id is not None
        for _ in range(30):
            out = self.c.cli(["squeue", "-j", str(job_id), "-h", "-o", "%T"])
            if "RUNNING" in out:
                break
            time.sleep(1)
        else:
            pytest.fail(f"job {job_id} never reached RUNNING state")
        self.c.cli(["scancel", str(job_id)])
        time.sleep(5)

        after = _scrape_counters(self.c, self.username)
        assert after.get(
            "spur_user_jobs_cancelled_total", 0
        ) > cancelled_before

    def test_timeout_job_increments_timeout_counter(self):
        """A job that hits its wall time limit increments timeout_total."""
        before = _scrape_counters(self.c, self.username)
        timeout_before = before.get("spur_user_jobs_timeout_total", 0)

        script = self.c.write_file(
            "karma-timeout.sh", "#!/bin/bash\nsleep 600\n"
        )
        self._submit_and_wait(
            ["-J", "karma-timeout", "-N", "1", "--time=1", script],
            timeout=180,
        )

        after = _scrape_counters(self.c, self.username)
        assert after.get(
            "spur_user_jobs_timeout_total", 0
        ) > timeout_before

    def test_counters_are_monotonic(self):
        """Submitting additional jobs never decreases counters."""
        before = _scrape_counters(self.c, self.username)

        script = self.c.write_file(
            "karma-mono.sh", "#!/bin/bash\nsleep 1\n"
        )
        self._submit_and_wait(
            ["-J", "karma-mono", "-N", "1", "--time=2", script]
        )

        after = _scrape_counters(self.c, self.username)
        for counter in KARMA_COUNTERS:
            assert after.get(counter, 0) >= before.get(counter, 0), (
                f"{counter} decreased: "
                f"{before.get(counter, 0)} -> {after.get(counter, 0)}"
            )
