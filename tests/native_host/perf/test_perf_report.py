# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from pathlib import Path

from perf.report import (
    DEFAULT_METRIC_POLICIES,
    MetricPolicy,
    PercentileStats,
    PerfSuiteResult,
    PerfTierResult,
    compare_perf_suites,
    delta_pct,
    format_comparison_report,
    load_suite_json,
    main,
    tier_mismatch_warnings,
    verdict_for_metric,
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
    assert delta_pct(0.0, 0.0) == 0.0
    assert delta_pct(10.0, 0.0) is None


def test_delta_pct_normal():
    assert delta_pct(110.0, 100.0) == 10.0
    assert delta_pct(90.0, 100.0) == -10.0


def test_verdict_higher_is_better():
    policy = MetricPolicy(True, 10.0, 0.0, True)
    assert verdict_for_metric(15.0, candidate=115.0, baseline=100.0, policy=policy) == "improved"
    assert verdict_for_metric(-15.0, candidate=85.0, baseline=100.0, policy=policy) == "regressed"
    assert verdict_for_metric(5.0, candidate=105.0, baseline=100.0, policy=policy) == "similar"
    assert verdict_for_metric(None, candidate=0.0, baseline=0.0, policy=policy) == "n/a"


def test_verdict_abs_floor_latency():
    policy = DEFAULT_METRIC_POLICIES["queue_wait_p50_s"]
    assert verdict_for_metric(100.0, candidate=2.0, baseline=1.0, policy=policy) == "similar"


def test_compare_perf_suites_skips_missing_baseline_tier():
    candidate = _suite("pr", _tier(100), _tier(500))
    baseline = _suite("nightly", _tier(100))
    comparisons = compare_perf_suites(candidate, baseline)
    assert set(comparisons) == {100}


def test_tier_mismatch_warnings():
    candidate = _suite("pr", _tier(100), _tier(500))
    baseline = _suite("nightly", _tier(100), _tier(1000))
    warnings = tier_mismatch_warnings(candidate, baseline)
    assert warnings == [
        "baseline missing tier N=500",
        "candidate missing tier N=1000",
    ]


def test_format_comparison_report_includes_warnings():
    candidate = _suite("pr", _tier(100), _tier(500))
    baseline = _suite("nightly", _tier(100))
    comparisons = compare_perf_suites(candidate, baseline)
    report = format_comparison_report(candidate, baseline, comparisons, threshold_pct=10.0)
    assert "## Warnings" in report
    assert "baseline missing tier N=500" in report


def test_format_comparison_report_splits_fail_gated_regressions():
    candidate = _suite(
        "pr",
        _tier(100, submit_tput_jps=70.0, queue_wait_p50=20.0),
    )
    baseline = _suite("nightly", _tier(100, submit_tput_jps=100.0, queue_wait_p50=5.0))
    comparisons = compare_perf_suites(candidate, baseline)
    report = format_comparison_report(candidate, baseline, comparisons, threshold_pct=10.0)
    assert "### Regressions (fail-gated)" in report
    assert "submit_tput_jps" in report
    assert "### Latency deltas (advisory, not fail-gated)" in report
    assert "queue_wait_p50_s" in report


def test_suite_json_roundtrip(tmp_path: Path):
    suite = _suite("roundtrip", _tier(50))
    path = tmp_path / "suite.json"
    write_suite_json(suite, path)
    loaded = load_suite_json(path)
    assert loaded.label == suite.label
    assert len(loaded.tiers) == 1
    assert loaded.tiers[0].tier_n == 50
    assert loaded.tiers[0].submit_tput_jps == suite.tiers[0].submit_tput_jps


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
