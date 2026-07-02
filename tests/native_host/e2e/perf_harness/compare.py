# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Compare two perf suite JSON results (e.g. PR vs nightly)."""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass

from .harness import PerfSuiteResult, PerfTierResult, load_suite_json


@dataclass
class MetricComparison:
    name: str
    candidate: float
    baseline: float
    delta_pct: float | None
    direction: str
    verdict: str


def _delta_pct(candidate: float, baseline: float) -> float | None:
    if baseline == 0:
        if candidate == 0:
            return 0.0
        return None
    return ((candidate - baseline) / baseline) * 100.0


def _verdict(delta_pct: float | None, *, higher_is_better: bool, threshold: float) -> str:
    if delta_pct is None:
        return "n/a"
    if higher_is_better:
        if delta_pct >= threshold:
            return "improved"
        if delta_pct <= -threshold:
            return "regressed"
        return "similar"
    if delta_pct <= -threshold:
        return "improved"
    if delta_pct >= threshold:
        return "regressed"
    return "similar"


def compare_tier_metrics(
    candidate: PerfTierResult,
    baseline: PerfTierResult,
    *,
    threshold_pct: float,
) -> list[MetricComparison]:
    specs = [
        ("submit_tput_jps", candidate.submit_tput_jps, baseline.submit_tput_jps, True),
        ("submitjob_rpc_avg_us", candidate.submitjob_rpc_avg_us, baseline.submitjob_rpc_avg_us, False),
        ("e2e_tput_jps", candidate.e2e_tput_jps, baseline.e2e_tput_jps, True),
        ("queue_wait_p50_s", candidate.queue_wait.p50, baseline.queue_wait.p50, False),
        ("turnaround_p50_s", candidate.turnaround.p50, baseline.turnaround.p50, False),
    ]
    out: list[MetricComparison] = []
    for name, cand, base, higher in specs:
        d = _delta_pct(cand, base)
        out.append(
            MetricComparison(
                name=name,
                candidate=cand,
                baseline=base,
                delta_pct=d,
                direction="higher is better" if higher else "lower is better",
                verdict=_verdict(d, higher_is_better=higher, threshold=threshold_pct),
            )
        )
    return out


def compare_perf_suites(
    candidate: PerfSuiteResult,
    baseline: PerfSuiteResult,
    *,
    threshold_pct: float = 10.0,
) -> dict[int, list[MetricComparison]]:
    baseline_by_n = {t.tier_n: t for t in baseline.tiers}
    comparisons: dict[int, list[MetricComparison]] = {}
    for cand_tier in candidate.tiers:
        base_tier = baseline_by_n.get(cand_tier.tier_n)
        if base_tier is None:
            continue
        comparisons[cand_tier.tier_n] = compare_tier_metrics(
            cand_tier, base_tier, threshold_pct=threshold_pct
        )
    return comparisons


def format_comparison_report(
    candidate: PerfSuiteResult,
    baseline: PerfSuiteResult,
    comparisons: dict[int, list[MetricComparison]],
    *,
    threshold_pct: float = 10.0,
) -> str:
    lines = [
        "# Spur perf comparison",
        "",
        f"**Candidate:** {candidate.label} (`{candidate.controller_addr}`)",
        f"**Baseline:** {baseline.label} (`{baseline.controller_addr}`)",
        f"**Regression threshold:** ±{threshold_pct:.0f}% vs baseline",
        "",
        "## Summary",
        "",
    ]

    regressions: list[str] = []
    improvements: list[str] = []
    for tier_n, metrics in sorted(comparisons.items()):
        for m in metrics:
            tag = f"N={tier_n} `{m.name}`: {m.candidate:.1f} vs {m.baseline:.1f}"
            if m.delta_pct is not None:
                tag += f" ({m.delta_pct:+.1f}%)"
            if m.verdict == "regressed":
                regressions.append(f"- {tag}")
            elif m.verdict == "improved":
                improvements.append(f"- {tag}")

    if regressions:
        lines.append("### Regressions")
        lines.extend(regressions)
        lines.append("")
    else:
        lines.append("_No regressions above threshold._")
        lines.append("")

    if improvements:
        lines.append("### Improvements")
        lines.extend(improvements)
        lines.append("")

    lines.extend(
        [
            "## By tier",
            "",
            "| Tier | Metric | Candidate | Baseline | Δ% | Verdict |",
            "|-----:|--------|----------:|---------:|---:|---------|",
        ]
    )
    for tier_n, metrics in sorted(comparisons.items()):
        for m in metrics:
            delta = "n/a" if m.delta_pct is None else f"{m.delta_pct:+.1f}"
            lines.append(
                f"| {tier_n} | {m.name} | {m.candidate:.1f} | {m.baseline:.1f} | {delta} | {m.verdict} |"
            )
    lines.append("")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Compare two perf suite JSON files.")
    parser.add_argument("candidate_json", help="JSON from candidate run (e.g. PR)")
    parser.add_argument("baseline_json", help="JSON from baseline run (e.g. nightly)")
    parser.add_argument("--out", help="Write markdown report to this path")
    parser.add_argument(
        "--threshold",
        type=float,
        default=float(__import__("os").environ.get("SPUR_PERF_COMPARE_THRESHOLD_PCT", "10")),
        help="Percent change treated as regression/improvement (default 10)",
    )
    parser.add_argument(
        "--fail-on-regression",
        action="store_true",
        help="Exit 1 if any metric regressed beyond threshold",
    )
    args = parser.parse_args(argv)

    candidate = load_suite_json(args.candidate_json)
    baseline = load_suite_json(args.baseline_json)
    comparisons = compare_perf_suites(candidate, baseline, threshold_pct=args.threshold)
    report = format_comparison_report(
        candidate, baseline, comparisons, threshold_pct=args.threshold
    )
    if args.out:
        from pathlib import Path

        Path(args.out).write_text(report + "\n", encoding="utf-8")
    print(report)
    if args.fail_on_regression:
        for metrics in comparisons.values():
            if any(m.verdict == "regressed" for m in metrics):
                return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
