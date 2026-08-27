# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Shared pytest hooks for the Spur test suites."""

import hashlib
import os
from pathlib import Path

import pytest

_TESTS_ROOT = Path(__file__).resolve().parent


def pytest_addoption(parser):
    group = parser.getgroup("spur-ci", "Spur CI test distribution")
    group.addoption(
        "--shard-count",
        type=int,
        default=1,
        help="Total number of shards to split the collected tests into (CI fan-out).",
    )
    group.addoption(
        "--shard-id",
        type=int,
        default=0,
        help="Zero-based id of the shard this process should run (0..shard-count-1).",
    )


def pytest_configure(config):
    count = config.getoption("--shard-count")
    index = config.getoption("--shard-id")
    if count < 1:
        raise pytest.UsageError(f"--shard-count must be >= 1, got {count}")
    if not 0 <= index < count:
        raise pytest.UsageError(
            f"--shard-id must be in [0, {count}), got {index}"
        )


def _shard_of(nodeid: str, count: int) -> int:
    # md5, not hash(): the built-in is salted per interpreter (PYTHONHASHSEED),
    # so it would assign the same test to different shards across processes.
    digest = hashlib.md5(nodeid.encode("utf-8")).hexdigest()
    return int(digest, 16) % count


@pytest.hookimpl(trylast=True)
def pytest_collection_modifyitems(config, items):
    """Deterministically keep only the tests belonging to this shard.

    Runs last so sharding divides the already marker-filtered pool
    (e.g. after ``-m gpu``), giving each shard an even slice of the pool.
    """
    count = config.getoption("--shard-count")
    if count <= 1:
        return

    index = config.getoption("--shard-id")
    selected = []
    deselected = []
    for item in items:
        bucket = selected if _shard_of(item.nodeid, count) == index else deselected
        bucket.append(item)

    if deselected:
        config.hook.pytest_deselected(items=deselected)
        items[:] = selected


def _running_full_suite(config) -> bool:
    for arg in config.args:
        path = Path(str(arg)).resolve()
        if path == _TESTS_ROOT \
                or path == _TESTS_ROOT / "native_host" / "e2e" \
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

    # WireGuard tests are ordinary native_host/e2e files gated by their fixtures
    # (they install WireGuard where missing and skip only where a data plane
    # can't be provided), so the base native_host rule covers them — no separate
    # WG branch or opt-in var.
    if "native_host" in parts and not os.environ.get("SPUR_TEST_NODES", "").strip():
        return True
    if "k8s" in parts and not _kubeconfig_available():
        return True
    return False
