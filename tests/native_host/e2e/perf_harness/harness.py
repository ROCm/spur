# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Native-host perf harness.

Deploys an ephemeral cluster via the E2E ``SpurCluster`` fixture (controller =
node 0), uploads ``perf_tests/run_perf.sh``, and runs tiers remotely on the
controller. Metric 2 (``SubmitJob`` RPC stats) comes from the shell script
(``sdiag --reset`` PRE/POST delta).

For human-readable output, :func:`format_perf_summary_report` prints a
markdown-style summary to stdout (use ``pytest -s``).
"""

from __future__ import annotations

import json
import os
import re
import shlex
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from cluster import SpurCluster

_DEFAULT_TIERS = "50"
_METRICS_KEY_RE = re.compile(r"^([A-Z0-9_]+)=(.*)$")
_PERCENTILE_RE = re.compile(
    r"^(QUEUE_WAIT_S|RUN_TIME_S|TURNAROUND_S)\s+min/p50/p95/p99/max=(.+)$"
)


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def perf_scripts_dir() -> Path:
    raw = os.environ.get("SPUR_PERF_SCRIPTS_DIR", "").strip()
    if raw:
        return Path(raw)
    return _repo_root() / "perf_tests"


def _parse_int_env(var_name: str, raw: str) -> int:
    try:
        return int(raw)
    except ValueError as e:
        raise ValueError(f"{var_name}: expected integer, got {raw!r}") from e


def _env_int(var_name: str, default: str) -> int:
    return _parse_int_env(var_name, os.environ.get(var_name, default))


def parse_tiers_from_env() -> list[int]:
    raw = os.environ.get("SPUR_PERF_TIERS", _DEFAULT_TIERS).strip()
    tiers: list[int] = []
    for part in raw.replace(",", " ").split():
        part = part.strip()
        if not part:
            continue
        tiers.append(_parse_int_env("SPUR_PERF_TIERS", part))
    return tiers or [50]


def _perf_parallel() -> int:
    return max(1, _env_int("SPUR_PERF_PARALLEL", "32"))


def _perf_sleep_s() -> int:
    return max(0, _env_int("SPUR_PERF_SLEEP", "0"))


@dataclass
class PercentileStats:
    min: float = 0.0
    p50: float = 0.0
    p95: float = 0.0
    p99: float = 0.0
    max: float = 0.0

    @classmethod
    def from_slash_values(cls, raw: str) -> PercentileStats:
        parts = [float(p) for p in raw.split("/")]
        if len(parts) != 5:
            return cls()
        return cls(min=parts[0], p50=parts[1], p95=parts[2], p99=parts[3], max=parts[4])


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


@dataclass
class PerfSuiteResult:
    label: str
    controller_addr: str
    node_hosts: list[str]
    node_names: list[str]
    sleep_s: int
    parallel: int
    tiers: list[PerfTierResult] = field(default_factory=list)


def parse_run_perf_metrics(stdout: str) -> dict[str, str]:
    """Parse the KEY=VALUE metrics block from ``run_perf.sh`` stdout."""
    metrics: dict[str, str] = {}
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith("SAMPLED="):
            for token in line.split():
                if "=" in token:
                    key, value = token.split("=", 1)
                    metrics[key] = value
            continue
        m = _PERCENTILE_RE.match(line)
        if m:
            metrics[m.group(1)] = m.group(2)
            continue
        m = _METRICS_KEY_RE.match(line)
        if m:
            metrics[m.group(1)] = m.group(2)
    return metrics


def _float_metric(metrics: dict[str, str], key: str, default: float = 0.0) -> float:
    raw = metrics.get(key, "")
    try:
        return float(raw)
    except ValueError:
        return default


def _int_metric(metrics: dict[str, str], key: str, default: int = 0) -> int:
    raw = metrics.get(key, "")
    try:
        return int(float(raw))
    except ValueError:
        return default


def _str_metric(metrics: dict[str, str], key: str, default: str = "") -> str:
    return metrics.get(key, default)


def tier_result_from_metrics(
    *,
    tier_n: int,
    sleep_s: int,
    parallel: int,
    metrics: dict[str, str],
) -> PerfTierResult:
    sampled_line = metrics.get("SAMPLED", "0")
    sampled = _int_metric({"SAMPLED": sampled_line}, "SAMPLED")
    completed = _int_metric(metrics, "COMPLETED_SAMPLED")
    failed = _int_metric(metrics, "NONCOMPLETED_SAMPLED")

    return PerfTierResult(
        tier_n=tier_n,
        sleep_s=sleep_s,
        parallel=parallel,
        accepted=_int_metric(metrics, "ACCEPTED"),
        submit_wall_s=_float_metric(metrics, "SUBMIT_WALL_S"),
        submit_tput_jps=_float_metric(metrics, "SUBMIT_TPUT_JPS"),
        submitjob_rpc_avg_us=_float_metric(metrics, "SUBMITJOB_RPC_AVG_US"),
        submitjob_rpc_count_delta=_int_metric(metrics, "SUBMITJOB_RPC_COUNT_DELTA"),
        submitjob_rpc_total_us_delta=_int_metric(metrics, "SUBMITJOB_RPC_TOTAL_US_DELTA"),
        release_wall_s=_float_metric(metrics, "RELEASE_WALL_S"),
        perf_job_name=_str_metric(metrics, "PERF_JOB_NAME"),
        drain_wall_s=_float_metric(metrics, "DRAIN_WALL_S"),
        total_wall_s=_float_metric(metrics, "TOTAL_WALL_S"),
        e2e_tput_jps=_float_metric(metrics, "E2E_TPUT_JPS"),
        peak_in_queue=_int_metric(metrics, "PEAK_IN_QUEUE"),
        sampled=sampled,
        completed_sampled=completed,
        noncompleted_sampled=failed,
        queue_wait=PercentileStats.from_slash_values(metrics.get("QUEUE_WAIT_S", "0/0/0/0/0")),
        run_time=PercentileStats.from_slash_values(metrics.get("RUN_TIME_S", "0/0/0/0/0")),
        turnaround=PercentileStats.from_slash_values(metrics.get("TURNAROUND_S", "0/0/0/0/0")),
    )


def _ensure_run_perf_script(cluster: SpurCluster) -> str:
    scripts = perf_scripts_dir()
    local_script = scripts / "run_perf.sh"
    if not local_script.is_file():
        raise FileNotFoundError(f"perf script not found: {local_script}")

    remote_path = f"{cluster.remote_dir}/run_perf.sh"
    cluster.nodes[0].upload(str(local_script), remote_path)
    cluster.nodes[0].exec(f"chmod +x {shlex.quote(remote_path)}")
    return remote_path


def _redeploy_cluster(cluster: SpurCluster) -> None:
    """Tear down and redeploy so the next perf tier starts on a cold controller."""
    overrides = cluster.config_overrides or None
    labels = cluster.agent_labels or None
    agent_as_root = cluster.agent_as_root
    cluster.teardown()
    cluster.deploy(
        config_overrides=overrides,
        agent_as_root=agent_as_root,
        agent_labels=labels,
    )


def run_native_perf_tier(
    cluster: SpurCluster,
    tier_n: int,
    *,
    script_path: str,
) -> PerfTierResult:
    """Run one perf tier on the controller node via ``run_perf.sh``."""
    sleep_s = _perf_sleep_s()
    parallel = min(_perf_parallel(), tier_n) if tier_n > 0 else 1

    cmd = (
        f"export SPUR_CONTROLLER_ADDR={shlex.quote(cluster.controller_addr)}; "
        f"export SPUR_CLI={shlex.quote(f'{cluster.bin_dir}/spur')}; "
        f"bash {shlex.quote(script_path)} {tier_n} {sleep_s} {parallel}"
    )
    stdout = cluster.nodes[0].exec(cmd)
    metrics = parse_run_perf_metrics(stdout)
    if "ACCEPTED" not in metrics:
        raise RuntimeError(
            f"run_perf.sh produced no metrics block for tier N={tier_n}\n"
            f"--- stdout ---\n{stdout}"
        )
    return tier_result_from_metrics(
        tier_n=tier_n,
        sleep_s=sleep_s,
        parallel=parallel,
        metrics=metrics,
    )


def run_native_perf_suite(cluster: SpurCluster, *, label: str | None = None) -> PerfSuiteResult:
    """Run all tiers from ``SPUR_PERF_*`` env against an ephemeral cluster.

    When multiple tier sizes are configured, the cluster is torn down and
    redeployed before each tier after the first so metrics are not skewed by
    accumulated job state from earlier tiers.
    """
    run_label = label or os.environ.get("SPUR_PERF_RUN_LABEL", "local")
    sleep_s = _perf_sleep_s()
    parallel = _perf_parallel()
    tiers_cfg = parse_tiers_from_env()

    script_path = _ensure_run_perf_script(cluster)
    results: list[PerfTierResult] = []
    for index, tier_n in enumerate(tiers_cfg):
        if index > 0:
            _redeploy_cluster(cluster)
            script_path = _ensure_run_perf_script(cluster)
        results.append(run_native_perf_tier(cluster, tier_n, script_path=script_path))

    return PerfSuiteResult(
        label=run_label,
        controller_addr=cluster.controller_addr,
        node_hosts=[n.host for n in cluster.nodes],
        node_names=list(cluster.node_names),
        sleep_s=sleep_s,
        parallel=parallel,
        tiers=results,
    )


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
        "**Harness:** `tests/native_host/e2e/perf_harness` + `perf_tests/run_perf.sh`",
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
    tiers = []
    for t in data.get("tiers", []):
        tiers.append(
            PerfTierResult(
                tier_n=t["tier_n"],
                sleep_s=t["sleep_s"],
                parallel=t["parallel"],
                accepted=t["accepted"],
                submit_wall_s=t["submit_wall_s"],
                submit_tput_jps=t["submit_tput_jps"],
                submitjob_rpc_avg_us=t["submitjob_rpc_avg_us"],
                submitjob_rpc_count_delta=t["submitjob_rpc_count_delta"],
                submitjob_rpc_total_us_delta=t["submitjob_rpc_total_us_delta"],
                release_wall_s=t.get("release_wall_s", 0.0),
                perf_job_name=t.get("perf_job_name", ""),
                drain_wall_s=t["drain_wall_s"],
                total_wall_s=t["total_wall_s"],
                e2e_tput_jps=t["e2e_tput_jps"],
                peak_in_queue=t["peak_in_queue"],
                sampled=t["sampled"],
                completed_sampled=t["completed_sampled"],
                noncompleted_sampled=t["noncompleted_sampled"],
                queue_wait=PercentileStats(**t["queue_wait"]),
                run_time=PercentileStats(**t["run_time"]),
                turnaround=PercentileStats(**t["turnaround"]),
            )
        )
    return PerfSuiteResult(
        label=data["label"],
        controller_addr=data["controller_addr"],
        node_hosts=data["node_hosts"],
        node_names=data["node_names"],
        sleep_s=data["sleep_s"],
        parallel=data["parallel"],
        tiers=tiers,
    )
