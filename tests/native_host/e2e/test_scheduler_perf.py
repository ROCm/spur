# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Optional scheduler perf test (native-host).

Uses the same ``cluster`` fixture as other native tests. After a successful run,
prints a markdown-style summary to stdout.
"""

from __future__ import annotations

import os

import pytest

from perf_harness.harness import (
    format_perf_summary_report,
    parse_tiers_from_env,
    run_native_perf_suite,
    write_suite_json,
)


@pytest.mark.perf
def test_scheduler_perf_submit_drain_latency(cluster):
    tiers = parse_tiers_from_env()
    assert tiers, "SPUR_PERF_TIERS produced no tiers"

    label = os.environ.get("SPUR_PERF_RUN_LABEL", "local")
    suite = run_native_perf_suite(cluster, label=label)

    for tier in suite.tiers:
        assert tier.accepted == tier.tier_n, (
            f"tier N={tier.tier_n}: expected {tier.tier_n} accepted submissions, "
            f"got {tier.accepted}"
        )

    sinfo_text = None
    try:
        sinfo_text = cluster.sinfo()
    except Exception:
        pass

    report = format_perf_summary_report(suite, sinfo_text=sinfo_text)
    print(report, flush=True)

    json_out = os.environ.get("SPUR_PERF_JSON_OUT", "").strip()
    if json_out:
        write_suite_json(suite, json_out)
