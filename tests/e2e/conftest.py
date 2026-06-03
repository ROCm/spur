# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Shared pytest hooks for the Spur E2E test suites."""

import os
from pathlib import Path


def _running_full_e2e_suite(config) -> bool:
    for arg in config.args:
        path = Path(str(arg)).resolve()
        if path.name == "e2e" and path.parent.name == "tests":
            return True
    return False


def _kubeconfig_available() -> bool:
    if os.environ.get("KUBECONFIG", "").strip():
        return True
    return Path.home().joinpath(".kube", "config").is_file()


def pytest_ignore_collect(collection_path, config):
    """Skip suites missing prerequisites when running ``pytest tests/e2e/``."""
    if not _running_full_e2e_suite(config):
        return False

    path = Path(str(collection_path))
    parts = path.parts

    if "native_host" in parts and not os.environ.get("SPUR_TEST_NODES", "").strip():
        return True
    if "k8s" in parts and not _kubeconfig_available():
        return True
    return False
