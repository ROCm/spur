# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for native karma Prometheus counters on /metrics/jobs-users-accts.

Each test uses a fresh per-test user (via the cluster's user fixture) so
counters start at zero and exact assertions are deterministic.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job


KARMA_COUNTERS = [
    "spur_karma_jobs_submitted_total",
    "spur_karma_jobs_completed_total",
    "spur_karma_jobs_failed_total",
    "spur_karma_jobs_timeout_total",
    "spur_karma_jobs_node_fail_total",
    "spur_karma_jobs_cancelled_total",
    "spur_karma_gpus_requested_total",
    "spur_karma_overflow_borrows_total",
    "spur_karma_walltime_requested_seconds_total",
    "spur_karma_walltime_actual_seconds_total",
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
    """Verify that native karma counters increment correctly with exact values."""

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

    def test_exact_counter_values_across_job_sequence(self):
        """Submit a controlled sequence and assert exact counter values.

        No --gres flag: the non-GPU test shard has no GPUs configured,
        so gpus_requested stays 0. GPU accounting is exercised on the
        GPU shard via the Crusoe live-cluster validation.
        """
        # 1. Completed job: 5min limit, sleeps 3s
        script_ok = self.c.write_file("karma-ok.sh", "#!/bin/bash\nsleep 3\n")
        self._submit_and_wait(["-J", "karma-ok", "-N", "1", "--time=5", script_ok])

        # 2. Failed job: 2min limit, exit 1
        script_fail = self.c.write_file("karma-fail.sh", "#!/bin/bash\nexit 1\n")
        self._submit_and_wait(["-J", "karma-fail", "-N", "1", "--time=2", script_fail])

        # 3. Cancelled job: 10min limit
        script_long = self.c.write_file("karma-long.sh", "#!/bin/bash\nsleep 600\n")
        job_id = parse_job_id(
            self.c.sbatch(["-J", "karma-cancel", "-N", "1", "--time=10", script_long])
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

        # 4. Timeout job: 1min limit
        self._submit_and_wait(
            ["-J", "karma-timeout", "-N", "1", "--time=1", script_long],
            timeout=180,
        )

        counters = _scrape_counters(self.c, self.username)
        assert counters["spur_karma_jobs_submitted_total"] == 4
        assert counters["spur_karma_jobs_completed_total"] == 1
        assert counters["spur_karma_jobs_failed_total"] == 1
        assert counters["spur_karma_jobs_timeout_total"] == 1
        assert counters["spur_karma_jobs_cancelled_total"] == 1
        assert counters["spur_karma_jobs_node_fail_total"] == 0
        assert counters["spur_karma_gpus_requested_total"] == 0
        assert counters["spur_karma_overflow_borrows_total"] == 0
        # 5min + 2min + 10min + 1min = 18min = 1080s
        assert counters["spur_karma_walltime_requested_seconds_total"] == 1080
        assert counters["spur_karma_walltime_actual_seconds_total"] > 0

    def test_type_declarations_are_counter(self):
        """All 10 karma metrics are declared as counter type."""
        script = self.c.write_file("karma-type.sh", "#!/bin/bash\nsleep 1\n")
        self._submit_and_wait(["-J", "karma-type", "-N", "1", "--time=2", script])

        body = _scrape_body(self.c)
        for counter in KARMA_COUNTERS:
            base = counter.removesuffix("_total")
            assert f"# TYPE {base} counter" in body, (
                f"{base} is not declared as counter type"
            )

    def test_zero_counters_still_present(self):
        """After at least one job, all 10 counters are present (even zeros)."""
        script = self.c.write_file("karma-zero.sh", "#!/bin/bash\nsleep 1\n")
        self._submit_and_wait(["-J", "karma-zero", "-N", "1", "--time=2", script])

        counters = _scrape_counters(self.c, self.username)
        for name in KARMA_COUNTERS:
            assert name in counters, f"{name} missing from metrics output"
