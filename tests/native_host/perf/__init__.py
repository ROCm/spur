# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Native-host scheduler perf tests (optional ``@pytest.mark.perf``)."""

from perf.harness import parse_tiers_from_env, run_native_perf_suite
from perf.report import (
    PerfSuiteResult,
    PerfTierResult,
    compare_perf_suites,
    format_comparison_report,
    format_perf_summary_report,
    load_suite_json,
    suite_to_dict,
    write_suite_json,
)

__all__ = [
    "PerfSuiteResult",
    "PerfTierResult",
    "compare_perf_suites",
    "format_comparison_report",
    "format_perf_summary_report",
    "load_suite_json",
    "parse_tiers_from_env",
    "run_native_perf_suite",
    "suite_to_dict",
    "write_suite_json",
]
