# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Scheduler stress harness (native SpurCluster).

Mirrors ``stress_tests/run_stress.sh`` metrics: parallel submit throughput,
queue drain, end-to-end throughput, and latency percentiles from
``scontrol show job`` timestamps (1-second resolution from the controller).

Submit and latency sampling run **on the controller node**: one Paramiko
``exec`` runs a driver script that uses ``xargs -P`` and small **worker**
scripts (no one-SSH-channel-per-job). This avoids Paramiko + ``sshd`` channel
limits when scaling submissions.

For human-readable output, :func:`format_stress_summary_report` builds a
markdown-style summary similar to ``silogen/rocm-cpu`` ``spur_stress_test/STRESS_SUMMARY.md``.
"""

from __future__ import annotations

import os
import re
import shlex
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from cluster import SpurCluster

_DEFAULT_TIERS = "50"


def _parse_int_env(var_name: str, raw: str) -> int:
    """Parse an integer from env or tier token; raise a clear error on failure."""
    try:
        return int(raw)
    except ValueError as e:
        raise ValueError(f"{var_name}: expected integer, got {raw!r}") from e


def _env_int(var_name: str, default: str) -> int:
    return _parse_int_env(var_name, os.environ.get(var_name, default))


def parse_tiers_from_env() -> list[int]:
    raw = os.environ.get("SPUR_STRESS_TIERS", _DEFAULT_TIERS).strip()
    tiers: list[int] = []
    for part in raw.replace(",", " ").split():
        part = part.strip()
        if not part:
            continue
        tiers.append(_parse_int_env("SPUR_STRESS_TIERS", part))
    if not tiers:
        tiers = [50]
    return tiers


def _stress_parallel() -> int:
    return max(1, _env_int("SPUR_STRESS_PARALLEL", "32"))


def _stress_sleep_s() -> int:
    return max(0, _env_int("SPUR_STRESS_SLEEP", "0"))


def _drain_timeout_s() -> int:
    return max(30, _env_int("SPUR_STRESS_DRAIN_TIMEOUT", "1200"))


def _sample_max() -> int:
    return max(1, _env_int("SPUR_STRESS_SAMPLE_MAX", "100"))


def _remote_scontrol_parallel() -> int:
    """Max parallel ``scontrol show job`` invocations on the controller (remote xargs -P)."""
    return max(1, _env_int("SPUR_STRESS_REMOTE_SCONTROL_PARALLEL", "16"))


# First column of default ``squeue`` output: plain id, array task ``123_4``,
# array range ``123_[1-10]``, or hybrid ``123[1-10]`` (see Slurm squeue JOBID).
_SQUEUE_JOBID_FIRST_COL = re.compile(
    r"^(?:\d+|\d+_\d+|\d+\[[^\]]+\]|\d+_\[[^\]]+\])(?:\.[\w.+-]+)?$"
)


def _first_field_looks_like_squeue_jobid(token: str) -> bool:
    if token in ("JOBID", "JOBID(S)"):
        return False
    if token.isdigit():
        return True
    return bool(_SQUEUE_JOBID_FIRST_COL.fullmatch(token))


def count_squeue_jobs(squeue_output: str) -> int:
    """Count job rows in ``squeue`` output (header line skipped)."""
    n = 0
    for line in squeue_output.splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split()
        if not parts:
            continue
        if _first_field_looks_like_squeue_jobid(parts[0]):
            n += 1
    return n


_JOB_STATE_RE = re.compile(r"JobState=(\S+)")
_TIMES_RE = re.compile(
    r"SubmitTime=(\S+)\s+StartTime=(\S+)\s+EndTime=(\S+)",
)


def parse_scontrol_show_job(output: str) -> dict:
    """Parse ``scontrol show job <id>`` lines for state and timestamps."""
    state = None
    submit = start = end = None
    for line in output.splitlines():
        m = _JOB_STATE_RE.search(line)
        if m:
            state = m.group(1)
        m = _TIMES_RE.search(line)
        if m:
            submit, start, end = m.group(1), m.group(2), m.group(3)
    return {"state": state, "submit": submit, "start": start, "end": end}


def _parse_ts_or_none(label: str) -> float | None:
    if not label or label == "N/A":
        return None
    try:
        return datetime.fromisoformat(label).timestamp()
    except ValueError:
        return None


def percentile_stats(values: list[float]) -> tuple[float, float, float, float, float]:
    """
    Return (min, p50, p95, p99, max) for a non-empty sorted sequence.
    Uses 1-based rank positions like the original bash (ceil(n * p / 100)).
    """
    if not values:
        return (0.0, 0.0, 0.0, 0.0, 0.0)
    s = sorted(values)
    n = len(s)

    def rank(p: int) -> float:
        idx = max(1, min(n, (n * p + 99) // 100))
        return float(s[idx - 1])

    return (float(s[0]), rank(50), rank(95), rank(99), float(s[-1]))


def percentile_block(name: str, values: list[float]) -> str:
    mn, p50, p95, p99, mx = percentile_stats(values)
    return f"{name}_S min/p50/p95/p99/max={mn:.0f}/{p50:.0f}/{p95:.0f}/{p99:.0f}/{mx:.0f}"


def _p50_p95_max(values: list[float]) -> tuple[float, float, float]:
    """Return (p50, p95, max) for latency tables (matches STRESS_SUMMARY style)."""
    if not values:
        return (0.0, 0.0, 0.0)
    _, p50, p95, _, mx = percentile_stats(values)
    return (p50, p95, mx)


def format_stress_summary_report(
    *,
    controller_addr: str,
    node_hosts: list[str],
    node_names: list[str] | None,
    sleep_s: int,
    results: list[StressTierResult],
    sinfo_text: str | None = None,
) -> str:
    """
    Build a markdown-style summary similar to ``silogen/rocm-cpu`` ``STRESS_SUMMARY.md``.

    Intended for printing to stdout (e.g. from pytest ``print(..., flush=True)``).
    """
    if not results:
        return "# Spur stress test — results summary\n\n_(no tier results — empty input.)_\n"
    when = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    names = node_names if node_names else node_hosts
    nodes_line = ", ".join(f"{h} ({n})" for h, n in zip(node_hosts, names))
    total_accepted = sum(r.accepted for r in results)
    n_nodes = len(node_hosts)
    par_env = _stress_parallel()
    max_eff_parallel = max((r.parallel for r in results), default=par_env)

    lines: list[str] = [
        "# Spur stress test — results summary",
        "",
        f"**Date:** {when}",
        f"**Controller:** `{controller_addr}`",
        f"**Nodes:** {n_nodes} — {nodes_line}",
        "**Harness:** native-host E2E (`tests/native_host/e2e/stress_harness`, remote `xargs` submit)",
        (
            "**Job type:** single-CPU batch (`#SBATCH -N 1 -n 1`, `exit 0`"
            + (f", `sleep {sleep_s}` before exit)" if sleep_s > 0 else ", instant `exit 0`)")
        ),
        f"**`SPUR_STRESS_PARALLEL` (env):** {par_env}; **effective parallel (max over tiers):** {max_eff_parallel}",
        f"**Total jobs accepted this run:** {total_accepted}",
        "",
        "## Cluster view (`sinfo`)",
        "",
    ]

    if sinfo_text and sinfo_text.strip():
        lines.append("```")
        lines.append(sinfo_text.rstrip())
        lines.append("```")
    else:
        lines.append("_`sinfo` not available (skipped or failed)._")
    lines.append("")

    lines.extend(
        [
            "## Throughput by tier",
            "",
            "| Jobs | Submit wall | **Submit tput** | Drain wall | Total wall | **E2E tput** | Peak in queue |",
            "|-----:|------------:|----------------:|-----------:|-----------:|-------------:|--------------:|",
        ]
    )
    for r in results:
        lines.append(
            f"| {r.accepted} | {r.submit_wall_s:.2f}s | **{r.submit_tput_jps:.0f} j/s** | "
            f"{r.drain_wall_s:.2f}s | {r.total_wall_s:.2f}s | **{r.e2e_tput_jps:.0f} j/s** | {r.peak_in_queue} |"
        )
    lines.append("")

    lines.extend(
        [
            "## Latency (controller timestamps, ~1s resolution)",
            "",
            "Sampled jobs use `scontrol show job` timestamps (same resolution caveats as the bash harness).",
            "",
        ]
    )
    for r in results:
        qw50, qw95, qwm = _p50_p95_max(r.queue_wait_values)
        rt50, rt95, rtm = _p50_p95_max(r.run_time_values)
        tt50, tt95, ttm = _p50_p95_max(r.turnaround_values)
        lines.append(f"### Tier N={r.tier_n} (sleep={r.sleep_s}s, sampled={r.sampled})")
        lines.append("")
        lines.append("| Metric | p50 | p95 | max |")
        lines.append("|--------|----:|----:|----:|")
        lines.append(
            f"| Queue wait (submit→start) | {qw50:.0f}s | {qw95:.0f}s | {qwm:.0f}s |"
        )
        lines.append(f"| Run time (start→end) | {rt50:.0f}s | {rt95:.0f}s | {rtm:.0f}s |")
        lines.append(
            f"| **Turnaround (submit→end)** | **{tt50:.0f}s** | **{tt95:.0f}s** | **{ttm:.0f}s** |"
        )
        lines.append("")

    max_submit = max((r.submit_tput_jps for r in results), default=0.0)
    max_e2e = max((r.e2e_tput_jps for r in results), default=0.0)
    max_peak = max((r.peak_in_queue for r in results), default=0)

    lines.extend(
        [
            "## Takeaways (this run)",
            "",
            f"- **Peak submit throughput observed:** ~{max_submit:.0f} jobs/s (submit phase only).",
            f"- **Peak end-to-end throughput observed:** ~{max_e2e:.0f} jobs/s "
            "(submit + drain for these tiers).",
            f"- **Peak concurrent jobs in default `squeue` view:** {max_peak}.",
            "- Controller timestamps are coarse (~1s); use for queueing behavior, not microbench.",
            "- Trivial batch scripts; real workloads add runtime, memory, and I/O not reflected here.",
            "",
        ]
    )

    return "\n".join(lines)


@dataclass
class StressTierResult:
    tier_n: int
    sleep_s: int
    parallel: int
    accepted: int
    submit_wall_s: float
    drain_wall_s: float
    total_wall_s: float
    peak_in_queue: int
    sampled: int
    queue_wait_values: list[float] = field(default_factory=list)
    run_time_values: list[float] = field(default_factory=list)
    turnaround_values: list[float] = field(default_factory=list)

    @property
    def submit_tput_jps(self) -> float:
        if self.submit_wall_s <= 0:
            return 0.0
        return self.accepted / self.submit_wall_s

    @property
    def e2e_tput_jps(self) -> float:
        if self.total_wall_s <= 0:
            return 0.0
        return self.accepted / self.total_wall_s

    def summary_lines(self) -> list[str]:
        lines = [
            f"tier_n={self.tier_n} sleep_s={self.sleep_s} parallel={self.parallel}",
            f"accepted={self.accepted} submit_wall_s={self.submit_wall_s:.3f} "
            f"submit_tput_jps={self.submit_tput_jps:.1f}",
            f"drain_wall_s={self.drain_wall_s:.3f} total_wall_s={self.total_wall_s:.3f} "
            f"e2e_tput_jps={self.e2e_tput_jps:.1f} peak_in_queue={self.peak_in_queue}",
            percentile_block("QUEUE_WAIT", self.queue_wait_values),
            percentile_block("RUN_TIME", self.run_time_values),
            percentile_block("TURNAROUND", self.turnaround_values),
            f"latency_samples={self.sampled}",
        ]
        return lines


def _stress_batch_script(sleep_s: int) -> str:
    lines = [
        "#!/bin/bash",
        "#SBATCH -N 1",
        "#SBATCH -n 1",
    ]
    if sleep_s > 0:
        lines.append(f"sleep {sleep_s}")
    lines.append("exit 0")
    return "\n".join(lines) + "\n"


_STRESS_SUBMIT_ONE_SH = r"""#!/usr/bin/env bash
set -uo pipefail
out=$($SBATCH -J stress -N 1 -o /dev/null -e /dev/null "$SCRIPT" 2>/dev/null) || true
id=$(echo "$out" | awk '/Submitted batch job/{print $NF}')
[ -n "$id" ] || exit 0
flock "$IDS_LOCK" bash -c 'echo "$1" >> "$2"' _ "$id" "$IDS_FILE"
exit 0
"""


_STRESS_LATENCY_ONE_SH = r"""#!/usr/bin/env bash
set -uo pipefail
jid="$1"
raw=$("$SCONTROL" show job "$jid" 2>/dev/null) || exit 0
sub=$(printf '%s' "$raw" | grep -oE 'SubmitTime=[^ ]+' | head -1 | cut -d= -f2)
sta=$(printf '%s' "$raw" | grep -oE 'StartTime=[^ ]+' | head -1 | cut -d= -f2)
end=$(printf '%s' "$raw" | grep -oE 'EndTime=[^ ]+' | head -1 | cut -d= -f2)
es=$(date -d "$sub" +%s 2>/dev/null || true)
et=$(date -d "$sta" +%s 2>/dev/null || true)
ee=$(date -d "$end" +%s 2>/dev/null || true)
[ -n "$es" ] && [ -n "$et" ] && [ -n "$ee" ] || exit 0
qw=$(awk -v es="$es" -v et="$et" 'BEGIN{printf "%f", et-es}')
rt=$(awk -v et="$et" -v ee="$ee" 'BEGIN{printf "%f", ee-et}')
tt=$(awk -v es="$es" -v ee="$ee" 'BEGIN{printf "%f", ee-es}')
flock "$OUT_LOCK" bash -c 'printf "%s %s %s\n" "$1" "$2" "$3" >> "$4"' _ "$qw" "$rt" "$tt" "$OUT_FILE"
exit 0
"""


def _remote_submit_driver_sh(
    *,
    controller_addr: str,
    bin_dir: str,
    script_path: str,
    ids_path: str,
    ids_lock: str,
    submit_one_path: str,
    tier_n: int,
    parallel: int,
) -> str:
    return f"""#!/usr/bin/env bash
set -uo pipefail
export SPUR_CONTROLLER_ADDR={shlex.quote(controller_addr)}
export PATH={shlex.quote(bin_dir)}:$PATH
export SBATCH={shlex.quote(f"{bin_dir}/sbatch")}
export SCRIPT={shlex.quote(script_path)}
export IDS_FILE={shlex.quote(ids_path)}
export IDS_LOCK={shlex.quote(ids_lock)}
TIER_N={tier_n}
PAR={parallel}
SUBMIT_ONE={shlex.quote(submit_one_path)}
: > "$IDS_FILE"
rm -f "$IDS_LOCK"
touch "$IDS_LOCK"
set +e
seq 1 "$TIER_N" | xargs -P "$PAR" -n1 "$SUBMIT_ONE"
set -euo pipefail
exit 0
"""


def _remote_latency_driver_sh(
    *,
    controller_addr: str,
    bin_dir: str,
    ids_path: str,
    out_path: str,
    out_lock: str,
    sample_one_path: str,
    sample_max: int,
    sctl_parallel: int,
) -> str:
    return f"""#!/usr/bin/env bash
set -uo pipefail
export SPUR_CONTROLLER_ADDR={shlex.quote(controller_addr)}
export PATH={shlex.quote(bin_dir)}:$PATH
export SCONTROL={shlex.quote(f"{bin_dir}/scontrol")}
export IDS_FILE={shlex.quote(ids_path)}
export OUT_FILE={shlex.quote(out_path)}
export OUT_LOCK={shlex.quote(out_lock)}
export SAMPLE_MAX={sample_max}
export SCTL_PAR={sctl_parallel}
SORTED="$IDS_FILE.sorted"
SAMPLE_ONE={shlex.quote(sample_one_path)}
if [ ! -s "$IDS_FILE" ]; then
  : > "$OUT_FILE"
  exit 0
fi
sort -n "$IDS_FILE" | uniq > "$SORTED"
n=$(wc -l < "$SORTED" | tr -d ' ')
stride=$(( n / SAMPLE_MAX ))
[ "$stride" -lt 1 ] && stride=1
: > "$OUT_FILE"
rm -f "$OUT_LOCK"
touch "$OUT_LOCK"
set +e
awk -v s="$stride" '(NR-1) % s == 0 {{print}}' "$SORTED" | xargs -P "$SCTL_PAR" -n1 "$SAMPLE_ONE"
set -euo pipefail
rm -f "$SORTED"
exit 0
"""


def _parse_remote_latency_lines(text: str) -> tuple[list[float], list[float], list[float], int]:
    qw_vals: list[float] = []
    rt_vals: list[float] = []
    tt_vals: list[float] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 3:
            continue
        try:
            qw, rt, tt = float(parts[0]), float(parts[1]), float(parts[2])
        except ValueError:
            continue
        qw_vals.append(qw)
        rt_vals.append(rt)
        tt_vals.append(tt)
    sampled = len(qw_vals)
    return qw_vals, rt_vals, tt_vals, sampled


def run_native_stress_tier(cluster: "SpurCluster", tier_n: int) -> StressTierResult:
    """Run one stress tier against an existing native-host cluster."""
    sleep_s = _stress_sleep_s()
    parallel = min(_stress_parallel(), tier_n) if tier_n > 0 else 1
    drain_timeout = _drain_timeout_s()
    sample_max = _sample_max()
    sctl_par = _remote_scontrol_parallel()

    script_path = cluster.write_file(
        f"stress-tier-{tier_n}.sh",
        _stress_batch_script(sleep_s),
    )

    rdir = cluster.remote_dir
    ids_path = f"{rdir}/stress-tier-{tier_n}-ids.txt"
    ids_lock = f"{ids_path}.lock"

    submit_one_path = cluster.write_file("stress_submit_one.sh", _STRESS_SUBMIT_ONE_SH)
    submit_driver_path = cluster.write_file(
        "stress_remote_submit.sh",
        _remote_submit_driver_sh(
            controller_addr=cluster.controller_addr,
            bin_dir=cluster.bin_dir,
            script_path=script_path,
            ids_path=ids_path,
            ids_lock=ids_lock,
            submit_one_path=submit_one_path,
            tier_n=tier_n,
            parallel=parallel,
        ),
    )

    t_sub_start = time.perf_counter()
    cluster.nodes[0].exec(f"bash {shlex.quote(submit_driver_path)}")
    t_sub_end = time.perf_counter()
    submit_wall = t_sub_end - t_sub_start

    ids_raw = cluster.nodes[0].read_file(ids_path)
    accepted = sum(1 for line in ids_raw.splitlines() if line.strip().isdigit())

    peak = 0
    t_drain_start = time.perf_counter()
    deadline = time.time() + drain_timeout
    while time.time() < deadline:
        # Default ``squeue`` (no ``-t all``) omits terminal jobs; ``-t all`` would
        # never drain to zero while cancelled/completed rows remain visible.
        nq = count_squeue_jobs(cluster.cli(["squeue"]))
        peak = max(peak, nq)
        if nq == 0:
            break
        time.sleep(1.0)
    else:
        raise TimeoutError(
            f"stress drain: active queue not empty after {drain_timeout}s "
            f"(last default squeue count={count_squeue_jobs(cluster.cli(['squeue']))})"
        )
    t_all_end = time.perf_counter()

    drain_wall = t_all_end - t_drain_start
    total_wall = t_all_end - t_sub_start

    out_path = f"{rdir}/stress-tier-{tier_n}-latency.txt"
    out_lock = f"{out_path}.lock"
    sample_one_path = cluster.write_file("stress_latency_sample_one.sh", _STRESS_LATENCY_ONE_SH)
    latency_driver_path = cluster.write_file(
        "stress_remote_latency.sh",
        _remote_latency_driver_sh(
            controller_addr=cluster.controller_addr,
            bin_dir=cluster.bin_dir,
            ids_path=ids_path,
            out_path=out_path,
            out_lock=out_lock,
            sample_one_path=sample_one_path,
            sample_max=sample_max,
            sctl_parallel=sctl_par,
        ),
    )
    cluster.nodes[0].exec(f"bash {shlex.quote(latency_driver_path)}")
    lat_raw = cluster.nodes[0].read_file(out_path)
    qw_vals, rt_vals, tt_vals, sampled = _parse_remote_latency_lines(lat_raw)

    return StressTierResult(
        tier_n=tier_n,
        sleep_s=sleep_s,
        parallel=parallel,
        accepted=accepted,
        submit_wall_s=submit_wall,
        drain_wall_s=drain_wall,
        total_wall_s=total_wall,
        peak_in_queue=peak,
        sampled=sampled,
        queue_wait_values=qw_vals,
        run_time_values=rt_vals,
        turnaround_values=tt_vals,
    )
