# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import pytest


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--perf-json",
        action="store",
        default=None,
        help="Write perf suite results JSON to this file path",
    )


@pytest.fixture
def perf_json_path(request: pytest.FixtureRequest) -> str | None:
    return request.config.getoption("--perf-json")
