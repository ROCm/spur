# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

from perf_harness.harness import parse_run_perf_metrics, tier_result_from_metrics


def test_parse_run_perf_metrics_block():
    stdout = """
================ Spur perf tier ================
==> Submitting 10 jobs...
    accepted=10  submit_throughput=40.0 jobs/s
TIER_N=10
JOB_SLEEP_S=0
SUBMITTERS=8
ACCEPTED=10
SUBMIT_WALL_S=0.250
SUBMIT_TPUT_JPS=40.0
SUBMITJOB_RPC_COUNT_DELTA=10
SUBMITJOB_RPC_TOTAL_US_DELTA=50000
SUBMITJOB_RPC_AVG_US=5000
DRAIN_WALL_S=1.000
TOTAL_WALL_S=1.250
E2E_TPUT_JPS=8.0
PEAK_IN_QUEUE=3
SAMPLED=5 COMPLETED_SAMPLED=5 NONCOMPLETED_SAMPLED=0
QUEUE_WAIT_S min/p50/p95/p99/max=0/0/1/1/1
RUN_TIME_S   min/p50/p95/p99/max=0/0/0/0/0
TURNAROUND_S min/p50/p95/p99/max=0/1/1/1/1
"""
    metrics = parse_run_perf_metrics(stdout)
    tier = tier_result_from_metrics(tier_n=10, sleep_s=0, parallel=8, metrics=metrics)
    assert tier.accepted == 10
    assert tier.submit_tput_jps == 40.0
    assert tier.submitjob_rpc_avg_us == 5000
    assert tier.queue_wait.p95 == 1.0
    assert tier.completed_sampled == 5
