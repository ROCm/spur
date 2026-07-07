# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Pure perf report types, JSON I/O, and suite comparison (no cluster access)."""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class PercentileStats:
    min: float = 0.0
    p50: float = 0.0
    p95: float = 0.0
    p99: float = 0.0
    max: float = 0.0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PercentileStats:
        return cls(
            min=float(data.get("min", 0)),
            p50=float(data.get("p50", 0)),
            p95=float(data.get("p95", 0)),
            p99=float(data.get("p99", 0)),
            max=float(data.get("max", 0)),
        )


@dataclass
class PerfTierResult:
    tier_n: int
    sleep_s: int
    parallel: int
    accepted: int
    submit_wall_s: float
    submit_tput_jps: float
    submitjob_rpc_avg_us: float
    submitjob_rpc_count_delta: int
    submitjob_rpc_total_us_delta: int
    release_wall_s: float
    perf_job_name: str
    drain_wall_s: float
    total_wall_s: float
    e2e_tput_jps: float
    peak_in_queue: int
    sampled: int
    completed_sampled: int
    noncompleted_sampled: int
    queue_wait: PercentileStats = field(default_factory=PercentileStats)
    run_time: PercentileStats = field(default_factory=PercentileStats)
    turnaround: PercentileStats = field(default_factory=PercentileStats)

    @classmethod
    def from_tier_json(
        cls,
        data: dict[str, Any],
        *,
        sleep_s: int,
        parallel: int,
    ) -> PerfTierResult:
        return cls(
            tier_n=int(data["tier_n"]),
            sleep_s=sleep_s,
            parallel=parallel,
            accepted=int(data["accepted"]),
            submit_wall_s=float(data["submit_wall_s"]),
            submit_tput_jps=float(data["submit_tput_jps"]),
            submitjob_rpc_avg_us=float(data["submitjob_rpc_avg_us"]),
            submitjob_rpc_count_delta=int(data["submitjob_rpc_count_delta"]),
            submitjob_rpc_total_us_delta=int(data["submitjob_rpc_total_us_delta"]),
            release_wall_s=float(data["release_wall_s"]),
            perf_job_name=str(data.get("perf_job_name", "")),
            drain_wall_s=float(data["drain_wall_s"]),
            total_wall_s=float(data["total_wall_s"]),
            e2e_tput_jps=float(data["e2e_tput_jps"]),
            peak_in_queue=int(data["peak_in_queue"]),
            sampled=int(data["sampled"]),
            completed_sampled=int(data["completed_sampled"]),
            noncompleted_sampled=int(data["noncompleted_sampled"]),
            queue_wait=PercentileStats.from_dict(data.get("queue_wait", {})),
            run_time=PercentileStats.from_dict(data.get("run_time", {})),
            turnaround=PercentileStats.from_dict(data.get("turnaround", {})),
        )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PerfTierResult:
        """Load a tier from suite JSON (``write_suite_json`` / ``asdict`` shape)."""
        return cls(
            tier_n=int(data["tier_n"]),
            sleep_s=int(data["sleep_s"]),
            parallel=int(data["parallel"]),
            accepted=int(data["accepted"]),
            submit_wall_s=float(data["submit_wall_s"]),
            submit_tput_jps=float(data["submit_tput_jps"]),
            submitjob_rpc_avg_us=float(data["submitjob_rpc_avg_us"]),
            submitjob_rpc_count_delta=int(data["submitjob_rpc_count_delta"]),
            submitjob_rpc_total_us_delta=int(data["submitjob_rpc_total_us_delta"]),
            release_wall_s=float(data.get("release_wall_s", 0.0)),
            perf_job_name=str(data.get("perf_job_name", "")),
            drain_wall_s=float(data["drain_wall_s"]),
            total_wall_s=float(data["total_wall_s"]),
            e2e_tput_jps=float(data["e2e_tput_jps"]),
            peak_in_queue=int(data["peak_in_queue"]),
            sampled=int(data["sampled"]),
            completed_sampled=int(data["completed_sampled"]),
            noncompleted_sampled=int(data["noncompleted_sampled"]),
            queue_wait=PercentileStats(**data["queue_wait"]),
            run_time=PercentileStats(**data["run_time"]),
            turnaround=PercentileStats(**data["turnaround"]),
        )


@dataclass
class PerfSuiteResult:
    label: str
    controller_addr: str
    node_hosts: list[str]
    node_names: list[str]
    sleep_s: int
    parallel: int
    tiers: list[PerfTierResult] = field(default_factory=list)


@dataclass
class MetricComparison:
    name: str
    candidate: float
    baseline: float
    delta_pct: float | None
    direction: str
    verdict: str
    fail_gate: bool


@dataclass(frozen=True)
class MetricPolicy:
    higher_is_better: bool
    threshold_pct: float
    abs_floor: float
    fail_gate: bool


DEFAULT_METRIC_POLICIES: dict[str, MetricPolicy] = {
    "submit_tput_jps": MetricPolicy(True, 10.0, 1.0, True),
    "submitjob_rpc_avg_us": MetricPolicy(False, 10.0, 50.0, True),
    "e2e_tput_jps": MetricPolicy(True, 10.0, 0.5, True),
    "queue_wait_p50_s": MetricPolicy(False, 10.0, 2.0, False),
    "turnaround_p50_s": MetricPolicy(False, 10.0, 2.0, False),
}


def delta_pct(candidate: float, baseline: float) -> float | None:
    if baseline == 0:
        if candidate == 0:
            return 0.0
        return None
    return ((candidate - baseline) / baseline) * 100.0


def verdict_for_metric(
    delta_pct: float | None,
    *,
    candidate: float,
    baseline: float,
    policy: MetricPolicy,
) -> str:
    if delta_pct is None:
        return "n/a"
    if abs(candidate - baseline) < policy.abs_floor:
        return "similar"
    if policy.higher_is_better:
        if delta_pct >= policy.threshold_pct:
            return "improved"
        if delta_pct <= -policy.threshold_pct:
            return "regressed"
        return "similar"
    if delta_pct <= -policy.threshold_pct:
        return "improved"
    if delta_pct >= policy.threshold_pct:
        return "regressed"
    return "similar"


def compare_tier_metrics(
    candidate: PerfTierResult,
    baseline: PerfTierResult,
    *,
    policies: dict[str, MetricPolicy] | None = None,
) -> list[MetricComparison]:
    cfg = policies or DEFAULT_METRIC_POLICIES
    specs = [
        ("submit_tput_jps", candidate.submit_tput_jps, baseline.submit_tput_jps),
        ("submitjob_rpc_avg_us", candidate.submitjob_rpc_avg_us, baseline.submitjob_rpc_avg_us),
        ("e2e_tput_jps", candidate.e2e_tput_jps, baseline.e2e_tput_jps),
        ("queue_wait_p50_s", candidate.queue_wait.p50, baseline.queue_wait.p50),
        ("turnaround_p50_s", candidate.turnaround.p50, baseline.turnaround.p50),
    ]
    out: list[MetricComparison] = []
    for name, cand, base in specs:
        policy = cfg[name]
        d = delta_pct(cand, base)
        out.append(
            MetricComparison(
                name=name,
                candidate=cand,
                baseline=base,
                delta_pct=d,
                direction="higher is better" if policy.higher_is_better else "lower is better",
                verdict=verdict_for_metric(d, candidate=cand, baseline=base, policy=policy),
                fail_gate=policy.fail_gate,
            )
        )
    return out


def compare_perf_suites(
    candidate: PerfSuiteResult,
    baseline: PerfSuiteResult,
    *,
    policies: dict[str, MetricPolicy] | None = None,
) -> dict[int, list[MetricComparison]]:
    baseline_by_n = {t.tier_n: t for t in baseline.tiers}
    comparisons: dict[int, list[MetricComparison]] = {}
    for cand_tier in candidate.tiers:
        base_tier = baseline_by_n.get(cand_tier.tier_n)
        if base_tier is None:
            continue
        comparisons[cand_tier.tier_n] = compare_tier_metrics(
            cand_tier, base_tier, policies=policies
        )
    return comparisons


def tier_mismatch_warnings(
    candidate: PerfSuiteResult,
    baseline: PerfSuiteResult,
) -> list[str]:
    cand_ns = {t.tier_n for t in candidate.tiers}
    base_ns = {t.tier_n for t in baseline.tiers}
    warnings: list[str] = []
    for tier_n in sorted(cand_ns - base_ns):
        warnings.append(f"baseline missing tier N={tier_n}")
    for tier_n in sorted(base_ns - cand_ns):
        warnings.append(f"candidate missing tier N={tier_n}")
    return warnings


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
        f"**Regression threshold:** ±{threshold_pct:.0f}% vs baseline (latency uses abs floor)",
        "",
    ]

    mismatch_warnings = tier_mismatch_warnings(candidate, baseline)
    if mismatch_warnings:
        lines.extend(["## Warnings", ""])
        lines.extend(f"- {warning}" for warning in mismatch_warnings)
        lines.append("")

    lines.extend(["## Summary", ""])

    fail_gated_regressions: list[str] = []
    advisory_regressions: list[str] = []
    improvements: list[str] = []
    for tier_n, metrics in sorted(comparisons.items()):
        for m in metrics:
            tag = f"N={tier_n} `{m.name}`: {m.candidate:.1f} vs {m.baseline:.1f}"
            if m.delta_pct is not None:
                tag += f" ({m.delta_pct:+.1f}%)"
            if m.verdict == "regressed":
                if m.fail_gate:
                    fail_gated_regressions.append(f"- {tag}")
                else:
                    advisory_regressions.append(f"- {tag}")
            elif m.verdict == "improved":
                improvements.append(f"- {tag}")

    if fail_gated_regressions:
        lines.append("### Regressions (fail-gated)")
        lines.extend(fail_gated_regressions)
        lines.append("")
    else:
        lines.append("_No fail-gated regressions above threshold._")
        lines.append("")

    if advisory_regressions:
        lines.append("### Latency deltas (advisory, not fail-gated)")
        lines.extend(advisory_regressions)
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


def format_perf_summary_report(suite: PerfSuiteResult, *, sinfo_text: str | None = None) -> str:
    when = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    names = suite.node_names or suite.node_hosts
    nodes_line = ", ".join(f"{h} ({n})" for h, n in zip(suite.node_hosts, names))
    total_accepted = sum(t.accepted for t in suite.tiers)

    lines: list[str] = [
        "# Spur perf test — results summary",
        "",
        f"**Label:** {suite.label}",
        f"**Date:** {when}",
        f"**Controller:** `{suite.controller_addr}` (node 0)",
        f"**Nodes:** {len(suite.node_hosts)} — {nodes_line}",
        "**Harness:** `tests/native_host/perf` + `perf/scripts/run_perf.sh`",
        (
            "**Job type:** single-CPU batch (`#SBATCH -N 1 -n 1`"
            + (f", `sleep {suite.sleep_s}` before exit)" if suite.sleep_s > 0 else ", instant `exit 0`)")
        ),
        f"**`SPUR_PERF_PARALLEL` (env):** {suite.parallel}",
        f"**Total jobs accepted:** {total_accepted}",
        "",
        "## Cluster view (`sinfo`)",
        "",
    ]

    if sinfo_text and sinfo_text.strip():
        lines.extend(["```", sinfo_text.rstrip(), "```"])
    else:
        lines.append("_`sinfo` not available (skipped or failed)._")
    lines.append("")

    lines.extend(
        [
            "## Throughput by tier",
            "",
            "| Jobs | Submit wall | **Submit tput** | **RPC avg µs** | Release wall | "
            "Drain wall | Total wall | **E2E tput** | Peak in queue |",
            "|-----:|------------:|----------------:|---------------:|-------------:|"
            "-----------:|-----------:|-------------:|--------------:|",
        ]
    )
    for t in suite.tiers:
        lines.append(
            f"| {t.accepted} | {t.submit_wall_s:.2f}s | **{t.submit_tput_jps:.0f} j/s** | "
            f"{t.submitjob_rpc_avg_us:.0f} | {t.release_wall_s:.2f}s | {t.drain_wall_s:.2f}s | "
            f"{t.total_wall_s:.2f}s | **{t.e2e_tput_jps:.0f} j/s** | {t.peak_in_queue} |"
        )
    lines.append("")

    lines.extend(
        [
            "## Latency (controller timestamps, ~1s resolution)",
            "",
            "Queue wait is scheduling delay on the controller (`StartTime − SubmitTime`).",
            "",
        ]
    )
    for t in suite.tiers:
        qw = t.queue_wait
        rt = t.run_time
        tt = t.turnaround
        job_name_suffix = f", job_name=`{t.perf_job_name}`" if t.perf_job_name else ""
        lines.extend(
            [
                f"### Tier N={t.tier_n} (sleep={t.sleep_s}s, sampled={t.sampled}{job_name_suffix})",
                "",
                "| Metric | p50 | p95 | max |",
                "|--------|----:|----:|----:|",
                f"| Queue wait (submit→start) | {qw.p50:.0f}s | {qw.p95:.0f}s | {qw.max:.0f}s |",
                f"| Run time (start→end) | {rt.p50:.0f}s | {rt.p95:.0f}s | {rt.max:.0f}s |",
                f"| **Turnaround (submit→end)** | **{tt.p50:.0f}s** | **{tt.p95:.0f}s** | **{tt.max:.0f}s** |",
                "",
            ]
        )

    if suite.tiers:
        max_submit = max(t.submit_tput_jps for t in suite.tiers)
        max_e2e = max(t.e2e_tput_jps for t in suite.tiers)
        max_rpc = max(t.submitjob_rpc_avg_us for t in suite.tiers)
        lines.extend(
            [
                "## Takeaways (this run)",
                "",
                f"- **Peak submit throughput:** ~{max_submit:.0f} jobs/s",
                f"- **Peak end-to-end throughput:** ~{max_e2e:.0f} jobs/s",
                f"- **Peak SubmitJob RPC avg (tier max):** {max_rpc:.0f} µs",
                "- Controller timestamps are ~1s resolution; not for microbench.",
                "",
            ]
        )

    return "\n".join(lines)


def suite_to_dict(suite: PerfSuiteResult) -> dict:
    return asdict(suite)


def write_suite_json(suite: PerfSuiteResult, path: str | Path) -> None:
    Path(path).write_text(json.dumps(suite_to_dict(suite), indent=2) + "\n", encoding="utf-8")


def load_suite_json(path: str | Path) -> PerfSuiteResult:
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    tiers = [PerfTierResult.from_dict(t) for t in data.get("tiers", [])]
    return PerfSuiteResult(
        label=data["label"],
        controller_addr=data["controller_addr"],
        node_hosts=data["node_hosts"],
        node_names=data["node_names"],
        sleep_s=data["sleep_s"],
        parallel=data["parallel"],
        tiers=tiers,
    )


def median_suite_from_runs(paths: list[str | Path], *, label: str) -> PerfSuiteResult:
    """Aggregate multiple suite JSON files by per-tier metric median."""
    suites = [load_suite_json(p) for p in paths]
    if not suites:
        raise ValueError("median_suite_from_runs: no input paths")
    template = suites[0]
    tier_ns = sorted({t.tier_n for s in suites for t in s.tiers})
    median_tiers: list[PerfTierResult] = []

    def _median(vals: list[float]) -> float:
        return float(statistics.median(vals))

    for tier_n in tier_ns:
        tier_rows = [t for s in suites for t in s.tiers if t.tier_n == tier_n]
        if not tier_rows:
            continue
        ref = tier_rows[0]
        median_tiers.append(
            PerfTierResult(
                tier_n=tier_n,
                sleep_s=ref.sleep_s,
                parallel=ref.parallel,
                accepted=int(_median([float(t.accepted) for t in tier_rows])),
                submit_wall_s=_median([t.submit_wall_s for t in tier_rows]),
                submit_tput_jps=_median([t.submit_tput_jps for t in tier_rows]),
                submitjob_rpc_avg_us=_median([t.submitjob_rpc_avg_us for t in tier_rows]),
                submitjob_rpc_count_delta=int(
                    _median([float(t.submitjob_rpc_count_delta) for t in tier_rows])
                ),
                submitjob_rpc_total_us_delta=int(
                    _median([float(t.submitjob_rpc_total_us_delta) for t in tier_rows])
                ),
                release_wall_s=_median([t.release_wall_s for t in tier_rows]),
                perf_job_name=ref.perf_job_name,
                drain_wall_s=_median([t.drain_wall_s for t in tier_rows]),
                total_wall_s=_median([t.total_wall_s for t in tier_rows]),
                e2e_tput_jps=_median([t.e2e_tput_jps for t in tier_rows]),
                peak_in_queue=int(_median([float(t.peak_in_queue) for t in tier_rows])),
                sampled=int(_median([float(t.sampled) for t in tier_rows])),
                completed_sampled=int(
                    _median([float(t.completed_sampled) for t in tier_rows])
                ),
                noncompleted_sampled=int(
                    _median([float(t.noncompleted_sampled) for t in tier_rows])
                ),
                queue_wait=PercentileStats(
                    min=_median([t.queue_wait.min for t in tier_rows]),
                    p50=_median([t.queue_wait.p50 for t in tier_rows]),
                    p95=_median([t.queue_wait.p95 for t in tier_rows]),
                    p99=_median([t.queue_wait.p99 for t in tier_rows]),
                    max=_median([t.queue_wait.max for t in tier_rows]),
                ),
                run_time=PercentileStats(
                    min=_median([t.run_time.min for t in tier_rows]),
                    p50=_median([t.run_time.p50 for t in tier_rows]),
                    p95=_median([t.run_time.p95 for t in tier_rows]),
                    p99=_median([t.run_time.p99 for t in tier_rows]),
                    max=_median([t.run_time.max for t in tier_rows]),
                ),
                turnaround=PercentileStats(
                    min=_median([t.turnaround.min for t in tier_rows]),
                    p50=_median([t.turnaround.p50 for t in tier_rows]),
                    p95=_median([t.turnaround.p95 for t in tier_rows]),
                    p99=_median([t.turnaround.p99 for t in tier_rows]),
                    max=_median([t.turnaround.max for t in tier_rows]),
                ),
            )
        )

    return PerfSuiteResult(
        label=label,
        controller_addr=template.controller_addr,
        node_hosts=template.node_hosts,
        node_names=template.node_names,
        sleep_s=template.sleep_s,
        parallel=template.parallel,
        tiers=median_tiers,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Compare two perf suite JSON files.")
    parser.add_argument("candidate_json", help="JSON from candidate run (e.g. PR)")
    parser.add_argument("baseline_json", help="JSON from baseline run (e.g. nightly)")
    parser.add_argument("--out", help="Write markdown report to this path")
    parser.add_argument(
        "--threshold",
        type=float,
        default=float(os.environ.get("SPUR_PERF_COMPARE_THRESHOLD_PCT", "10")),
        help="Percent change treated as regression/improvement (default 10)",
    )
    parser.add_argument(
        "--fail-on-regression",
        action="store_true",
        help="Exit 1 if any fail-gated metric regressed beyond threshold",
    )
    parser.add_argument(
        "--candidate-runs",
        nargs="*",
        help="Optional extra candidate JSON paths; median used before compare",
    )
    parser.add_argument(
        "--baseline-runs",
        nargs="*",
        help="Optional extra baseline JSON paths; median used before compare",
    )
    args = parser.parse_args(argv)

    if args.candidate_runs:
        paths = [args.candidate_json, *args.candidate_runs]
        candidate = median_suite_from_runs(paths, label=load_suite_json(args.candidate_json).label)
    else:
        candidate = load_suite_json(args.candidate_json)

    if args.baseline_runs:
        paths = [args.baseline_json, *args.baseline_runs]
        baseline = median_suite_from_runs(paths, label=load_suite_json(args.baseline_json).label)
    else:
        baseline = load_suite_json(args.baseline_json)

    policies = {
        name: MetricPolicy(
            p.higher_is_better,
            args.threshold,
            p.abs_floor,
            p.fail_gate,
        )
        for name, p in DEFAULT_METRIC_POLICIES.items()
    }
    comparisons = compare_perf_suites(candidate, baseline, policies=policies)
    report = format_comparison_report(
        candidate, baseline, comparisons, threshold_pct=args.threshold
    )
    if args.out:
        Path(args.out).write_text(report + "\n", encoding="utf-8")
    print(report)
    if args.fail_on_regression:
        for metrics in comparisons.values():
            if any(m.verdict == "regressed" and m.fail_gate for m in metrics):
                return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
