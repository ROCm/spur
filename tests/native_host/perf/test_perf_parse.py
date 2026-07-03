# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import json

import pytest

from perf.harness import parse_run_perf_stdout, parse_tiers_from_env
from perf.report import PerfTierResult


def test_parse_run_perf_stdout_json():
    tier_json = {
        "tier_n": 10,
        "accepted": 10,
        "submit_wall_s": 0.25,
        "submit_tput_jps": 40.0,
        "submitjob_rpc_avg_us": 5000,
        "submitjob_rpc_count_delta": 10,
        "submitjob_rpc_total_us_delta": 50000,
        "release_wall_s": 0.05,
        "perf_job_name": "spur_perf_test",
        "drain_wall_s": 1.0,
        "total_wall_s": 1.25,
        "e2e_tput_jps": 8.0,
        "peak_in_queue": 3,
        "sampled": 5,
        "completed_sampled": 5,
        "noncompleted_sampled": 0,
        "queue_wait": {"min": 0, "p50": 0, "p95": 1, "p99": 1, "max": 1},
        "run_time": {"min": 0, "p50": 0, "p95": 0, "p99": 0, "max": 0},
        "turnaround": {"min": 0, "p50": 1, "p95": 1, "p99": 1, "max": 1},
    }
    stdout = f"==> done\nPERF_METRICS_JSON={json.dumps(tier_json)}\n"
    parsed = parse_run_perf_stdout(stdout)
    tier = PerfTierResult.from_tier_json(parsed, sleep_s=0, parallel=8)
    assert tier.accepted == 10
    assert tier.submit_tput_jps == 40.0
    assert tier.submitjob_rpc_avg_us == 5000
    assert tier.release_wall_s == 0.05
    assert tier.perf_job_name == "spur_perf_test"
    assert tier.queue_wait.p95 == 1.0
    assert tier.completed_sampled == 5


def test_parse_tiers_from_env_rejects_non_positive(monkeypatch):
    monkeypatch.setenv("SPUR_PERF_TIERS", "50 0")
    with pytest.raises(ValueError, match="positive job count"):
        parse_tiers_from_env()
