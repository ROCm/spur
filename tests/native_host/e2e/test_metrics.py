# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the spurctld OpenMetrics endpoint.

The metrics server binds to loopback by default, so these use the
metrics_cluster fixture, which sets `[metrics] bind = "all"`. It leaves
`high_cardinality` off so the gating on /metrics/jobs-users-accts stays
observable.
"""

import re
import time

import pytest

from cluster import METRICS_PORT, parse_job_id, wait_job_state

# /metrics is an alias for /metrics/jobs.
ROUTE_FAMILIES = {
    "/metrics": ["spur_jobs_pending", "spur_jobs_running"],
    "/metrics/jobs": [
        "spur_jobs_pending",
        "spur_jobs_running",
        "spur_jobs_completed",
        "spur_jobs_cpus_alloc",
        "spur_jobs_gpus_alloc",
    ],
    "/metrics/nodes": [
        "spur_nodes_cpus",
        "spur_nodes_cpus_alloc",
        "spur_nodes_memory_bytes",
    ],
    "/metrics/partitions": ["spur_partitions", "spur_partition_nodes"],
    "/metrics/rpc": ["spur_rpc_stats"],
    "/metrics/scheduler": ["spur_scheduler_info", "spur_scheduler_cycle_last_time_us"],
}


def _scrape(cluster, path: str) -> str:
    status, body = cluster.http_get(path, port=METRICS_PORT)
    assert status == 200, f"GET {path} returned {status}:\n{body}"
    return body


def _gauge(body: str, name: str) -> float:
    match = re.search(rf"^{re.escape(name)}\s+([0-9.eE+-]+)$", body, re.MULTILINE)
    assert match, f"metric {name} missing from scrape:\n{body}"
    return float(match.group(1))


@pytest.fixture
def scrapeable(metrics_cluster):
    status, body = metrics_cluster.http_get("/metrics", port=METRICS_PORT)
    if status == 0:
        pytest.skip(f"metrics endpoint unreachable on {METRICS_PORT}: {body.strip()}")
    return metrics_cluster


class TestMetricsRoutes:
    @pytest.mark.parametrize("path,families", sorted(ROUTE_FAMILIES.items()))
    def test_route_exports_its_families(self, scrapeable, path, families):
        body = _scrape(scrapeable, path)
        for family in families:
            assert family in body, f"{path} is missing {family}:\n{body}"

    @pytest.mark.parametrize("path", sorted(ROUTE_FAMILIES))
    def test_scrape_terminates_with_eof(self, scrapeable, path):
        """OpenMetrics requires the trailing `# EOF`; without it a scraper
        treats the payload as truncated."""
        body = _scrape(scrapeable, path)
        assert body.endswith("# EOF\n"), (
            f"{path} must end with an OpenMetrics EOF marker, ends with "
            f"{body[-40:]!r}"
        )

    def test_metrics_alias_matches_jobs_route(self, scrapeable):
        alias = _scrape(scrapeable, "/metrics")
        jobs = _scrape(scrapeable, "/metrics/jobs")
        assert set(re.findall(r"^# TYPE (\S+)", alias, re.MULTILINE)) == set(
            re.findall(r"^# TYPE (\S+)", jobs, re.MULTILINE)
        ), "/metrics must expose the same families as /metrics/jobs"

    def test_node_gauges_match_the_cluster_size(self, scrapeable):
        body = _scrape(scrapeable, "/metrics/nodes")
        cpus = _gauge(body, "spur_nodes_cpus")
        assert cpus >= 64 * len(scrapeable.node_names), (
            f"spur_nodes_cpus ({cpus}) is below the configured cluster size"
        )


class TestLiveGauges:
    def test_running_gauge_tracks_a_job(self, scrapeable):
        before = _gauge(_scrape(scrapeable, "/metrics/jobs"), "spur_jobs_running")

        script = scrapeable.write_file("metrics-job.sh", "#!/bin/bash\nsleep 120\n")
        job_id = parse_job_id(scrapeable.sbatch(["-J", "metrics-job", script]))
        assert job_id is not None

        try:
            wait_job_state(scrapeable, job_id, "R", timeout=90)

            deadline = time.time() + 30
            while time.time() < deadline:
                during = _gauge(
                    _scrape(scrapeable, "/metrics/jobs"), "spur_jobs_running"
                )
                if during > before:
                    break
                time.sleep(2)
            else:
                raise AssertionError(
                    f"spur_jobs_running did not rise above {before} while job "
                    f"{job_id} was running"
                )
        finally:
            scrapeable.cli_allow_fail(["scancel", str(job_id)])

        deadline = time.time() + 60
        while time.time() < deadline:
            after = _gauge(_scrape(scrapeable, "/metrics/jobs"), "spur_jobs_running")
            if after <= before:
                return
            time.sleep(2)
        raise AssertionError(
            f"spur_jobs_running stayed above {before} after job {job_id} ended"
        )

    def test_pending_gauge_tracks_a_held_job(self, scrapeable):
        before = _gauge(_scrape(scrapeable, "/metrics/jobs"), "spur_jobs_hold")

        script = scrapeable.write_file("metrics-held.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            scrapeable.sbatch(["-J", "metrics-held", "-H", script])
        )
        assert job_id is not None

        try:
            wait_job_state(scrapeable, job_id, "PD", timeout=30)
            deadline = time.time() + 30
            while time.time() < deadline:
                if _gauge(
                    _scrape(scrapeable, "/metrics/jobs"), "spur_jobs_hold"
                ) > before:
                    return
                time.sleep(2)
            raise AssertionError(
                f"spur_jobs_hold did not rise above {before} for held job {job_id}"
            )
        finally:
            scrapeable.cli_allow_fail(["scancel", str(job_id)])

    def test_rpc_counters_advance_with_traffic(self, scrapeable):
        scrapeable.squeue_all()
        first = _scrape(scrapeable, "/metrics/rpc")
        for _ in range(5):
            scrapeable.squeue_all()
        second = _scrape(scrapeable, "/metrics/rpc")
        assert first != second, (
            "RPC counters must advance after serving more requests:\n"
            f"{second}"
        )


class TestHighCardinalityGating:
    def test_jobs_users_accts_is_404_by_default(self, scrapeable):
        status, body = scrapeable.http_get(
            "/metrics/jobs-users-accts", port=METRICS_PORT
        )
        assert status == 404, (
            f"per-user metrics must be off by default, got {status}:\n{body}"
        )
        assert "high_cardinality" in body, f"expected a config hint:\n{body}"

    def test_jobs_users_accts_served_when_enabled(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.curl_preflight()
        cluster.start(
            config_overrides={
                "metrics": {"bind": "all", "high_cardinality": True},
            }
        )

        status, body = cluster.http_get(
            "/metrics/jobs-users-accts", port=METRICS_PORT
        )
        if status == 0:
            pytest.skip(f"metrics endpoint unreachable: {body.strip()}")
        assert status == 200, (
            f"high_cardinality = true must expose the route, got {status}:\n{body}"
        )
        assert body.endswith("# EOF\n"), (
            f"scrape must end with an OpenMetrics EOF marker:\n{body[-40:]!r}"
        )
