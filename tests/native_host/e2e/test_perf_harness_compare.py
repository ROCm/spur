# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from pathlib import Path

from perf_harness.compare import (
    _delta_pct,
    _tier_mismatch_warnings,
    _verdict,
    compare_perf_suites,
    format_comparison_report,
    main,
)
from perf_harness.harness import (
    PercentileStats,
    PerfSuiteResult,
    PerfTierResult,
    write_suite_json,
)


def _tier(
    tier_n: int,
    *,
    submit_tput_jps: float = 100.0,
    submitjob_rpc_avg_us: float = 500.0,
    e2e_tput_jps: float = 50.0,
    queue_wait_p50: float = 5.0,
    turnaround_p50: float = 10.0,
) -> PerfTierResult:
    return PerfTierResult(
        tier_n=tier_n,
        sleep_s=0,
        parallel=32,
        accepted=tier_n,
        submit_wall_s=1.0,
        submit_tput_jps=submit_tput_jps,
        submitjob_rpc_avg_us=submitjob_rpc_avg_us,
        submitjob_rpc_count_delta=tier_n,
        submitjob_rpc_total_us_delta=int(submitjob_rpc_avg_us * tier_n),
        release_wall_s=0.1,
        perf_job_name=f"spur_perf_{tier_n}",
        drain_wall_s=1.0,
        total_wall_s=2.0,
        e2e_tput_jps=e2e_tput_jps,
        peak_in_queue=0,
        sampled=10,
        completed_sampled=10,
        noncompleted_sampled=0,
        queue_wait=PercentileStats(p50=queue_wait_p50),
        turnaround=PercentileStats(p50=turnaround_p50),
    )


def _suite(label: str, *tiers: PerfTierResult) -> PerfSuiteResult:
    return PerfSuiteResult(
        label=label,
        controller_addr="http://127.0.0.1:6817",
        node_hosts=["127.0.0.1"],
        node_names=["node0"],
        sleep_s=0,
        parallel=32,
        tiers=list(tiers),
    )


def test_delta_pct_zero_baseline():
    assert _delta_pct(0.0, 0.0) == 0.0
    assert _delta_pct(10.0, 0.0) is None


def test_delta_pct_normal():
    assert _delta_pct(110.0, 100.0) == 10.0
    assert _delta_pct(90.0, 100.0) == -10.0


def test_verdict_higher_is_better():
    threshold = 10.0
    assert _verdict(15.0, higher_is_better=True, threshold=threshold) == "improved"
    assert _verdict(-15.0, higher_is_better=True, threshold=threshold) == "regressed"
    assert _verdict(5.0, higher_is_better=True, threshold=threshold) == "similar"
    assert _verdict(None, higher_is_better=True, threshold=threshold) == "n/a"


def test_verdict_lower_is_better():
    threshold = 10.0
    assert _verdict(-15.0, higher_is_better=False, threshold=threshold) == "improved"
    assert _verdict(15.0, higher_is_better=False, threshold=threshold) == "regressed"
    assert _verdict(5.0, higher_is_better=False, threshold=threshold) == "similar"


def test_compare_perf_suites_skips_missing_baseline_tier():
    candidate = _suite("pr", _tier(100), _tier(500))
    baseline = _suite("nightly", _tier(100))
    comparisons = compare_perf_suites(candidate, baseline, threshold_pct=10.0)
    assert set(comparisons) == {100}


def test_tier_mismatch_warnings():
    candidate = _suite("pr", _tier(100), _tier(500))
    baseline = _suite("nightly", _tier(100), _tier(1000))
    warnings = _tier_mismatch_warnings(candidate, baseline)
    assert warnings == [
        "baseline missing tier N=500",
        "candidate missing tier N=1000",
    ]


def test_format_comparison_report_includes_warnings():
    candidate = _suite("pr", _tier(100), _tier(500))
    baseline = _suite("nightly", _tier(100))
    comparisons = compare_perf_suites(candidate, baseline, threshold_pct=10.0)
    report = format_comparison_report(candidate, baseline, comparisons, threshold_pct=10.0)
    assert "## Warnings" in report
    assert "baseline missing tier N=500" in report


def test_main_fail_on_regression(tmp_path: Path):
    candidate = _suite("pr", _tier(100, submit_tput_jps=70.0))
    baseline = _suite("nightly", _tier(100, submit_tput_jps=100.0))
    cand_path = tmp_path / "candidate.json"
    base_path = tmp_path / "baseline.json"
    write_suite_json(candidate, cand_path)
    write_suite_json(baseline, base_path)

    assert (
        main(
            [
                str(cand_path),
                str(base_path),
                "--fail-on-regression",
                "--threshold",
                "10",
            ]
        )
        == 1
    )


def test_main_no_regression_exit_zero(tmp_path: Path):
    candidate = _suite("pr", _tier(100, submit_tput_jps=105.0))
    baseline = _suite("nightly", _tier(100, submit_tput_jps=100.0))
    cand_path = tmp_path / "candidate.json"
    base_path = tmp_path / "baseline.json"
    write_suite_json(candidate, cand_path)
    write_suite_json(baseline, base_path)

    assert (
        main(
            [
                str(cand_path),
                str(base_path),
                "--fail-on-regression",
                "--threshold",
                "10",
            ]
        )
        == 0
    )
