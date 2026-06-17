# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Uses the same ``cluster`` fixture as other native tests.
After a successful run, prints a markdown-style summary to stdout.
"""

from __future__ import annotations

import pytest

from stress_harness.harness import (
    format_stress_summary_report,
    parse_tiers_from_env,
    run_native_stress_tier,
)


@pytest.mark.stress
def test_scheduler_stress_submit_drain_latency(cluster):
    tiers = parse_tiers_from_env()
    assert tiers, "SPUR_STRESS_TIERS produced no tiers"

    results = []
    for tier_n in tiers:
        if tier_n <= 0:
            pytest.fail(f"invalid SPUR_STRESS_TIERS entry: {tier_n}")
        result = run_native_stress_tier(cluster, tier_n)
        assert result.accepted == tier_n, (
            f"tier N={tier_n}: expected {tier_n} accepted submissions, "
            f"got {result.accepted}"
        )
        results.append(result)

    sinfo_text = None
    try:
        sinfo_text = cluster.sinfo()
    except Exception:
        pass

    report = format_stress_summary_report(
        controller_addr=cluster.controller_addr,
        node_hosts=[n.host for n in cluster.nodes],
        node_names=list(cluster.node_names),
        sleep_s=results[0].sleep_s,
        results=results,
        sinfo_text=sinfo_text,
    )
    print(report, flush=True)
