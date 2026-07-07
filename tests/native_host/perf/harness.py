# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Native-host perf test orchestration (cluster deploy + remote run_perf.sh).

Pure JSON parsing, comparison, and reporting live in :mod:`perf.report`.
"""

from __future__ import annotations

import json
import os
import shlex
from pathlib import Path
from typing import TYPE_CHECKING

from perf.report import PerfSuiteResult, PerfTierResult

if TYPE_CHECKING:
    from cluster import SpurCluster

_DEFAULT_TIERS = "50"
_PERF_METRICS_PREFIX = "PERF_METRICS_JSON="


def perf_scripts_dir() -> Path:
    raw = os.environ.get("SPUR_PERF_SCRIPTS_DIR", "").strip()
    if raw:
        return Path(raw)
    return Path(__file__).resolve().parent / "scripts"


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
        value = _parse_int_env("SPUR_PERF_TIERS", part)
        if value <= 0:
            raise ValueError(
                f"SPUR_PERF_TIERS: expected positive job count, got {value!r}"
            )
        tiers.append(value)
    return tiers or [50]


def _perf_parallel() -> int:
    return max(1, _env_int("SPUR_PERF_PARALLEL", "32"))


def _perf_sleep_s() -> int:
    return max(0, _env_int("SPUR_PERF_SLEEP", "0"))


def parse_run_perf_stdout(stdout: str) -> dict:
    """Extract tier metrics JSON emitted by ``run_perf.sh``."""
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if line.startswith(_PERF_METRICS_PREFIX):
            payload = line[len(_PERF_METRICS_PREFIX) :]
            return json.loads(payload)
    raise RuntimeError(
        "run_perf.sh produced no PERF_METRICS_JSON line\n"
        f"--- stdout ---\n{stdout}"
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
    tier_json = parse_run_perf_stdout(stdout)
    return PerfTierResult.from_tier_json(tier_json, sleep_s=sleep_s, parallel=parallel)


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
