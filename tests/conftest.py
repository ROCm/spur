# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Shared pytest hooks for the Spur test suites."""

import os
from pathlib import Path

_TESTS_ROOT = Path(__file__).resolve().parent


def _running_full_suite(config) -> bool:
    for arg in config.args:
        path = Path(str(arg)).resolve()
        if path == _TESTS_ROOT or path == _TESTS_ROOT / "native_host" / "e2e" \
                or path == _TESTS_ROOT / "k8s" / "e2e":
            return True
    return False


def _kubeconfig_available() -> bool:
    if os.environ.get("KUBECONFIG", "").strip():
        return True
    return Path.home().joinpath(".kube", "config").is_file()


def pytest_ignore_collect(collection_path, config):
    """Skip suites missing prerequisites when running from the tests/ root."""
    if not _running_full_suite(config):
        return False

    path = Path(str(collection_path))
    parts = path.parts

    if "native_host" in parts and not os.environ.get("SPUR_TEST_NODES", "").strip():
        return True
    if "k8s" in parts and not _kubeconfig_available():
        return True
    return False
