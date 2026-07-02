# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Native-host perf harness (optional ``@pytest.mark.perf`` tests)."""

from .compare import compare_perf_suites, format_comparison_report
from .harness import (
    PerfSuiteResult,
    PerfTierResult,
    format_perf_summary_report,
    parse_tiers_from_env,
    run_native_perf_suite,
    suite_to_dict,
    write_suite_json,
)

__all__ = [
    "PerfSuiteResult",
    "PerfTierResult",
    "compare_perf_suites",
    "format_comparison_report",
    "format_perf_summary_report",
    "parse_tiers_from_env",
    "run_native_perf_suite",
    "suite_to_dict",
    "write_suite_json",
]
