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
    digest = hashlib.md5(nodeid.encode("utf-8"), usedforsecurity=False).hexdigest()
    return int(digest, 16) % count


def order_k0s_last(items: list) -> list:
    """Stable partition: non-k0s items first, k0s items after, each in original order.

    `k0s reset` can take a node's network down, so its fallout must only reach
    other k0s tests, never the rest of the shard.
    """
    plain = [item for item in items if item.get_closest_marker("k0s") is None]
    k0s = [item for item in items if item.get_closest_marker("k0s") is not None]
    return plain + k0s


@pytest.hookimpl(trylast=True)
def pytest_collection_modifyitems(config, items):
    """Keep only this shard's tests, then run the k0s ones last.

    Runs last so sharding divides the already marker-filtered pool
    (e.g. after ``-m gpu``), giving each shard an even slice of the pool, and so
    every other conftest has already applied its auto-markers (e.g. ``k0s``).
    Ordering comes after sharding: membership depends only on the node id.
    """
    count = config.getoption("--shard-count")
    if count > 1:
        index = config.getoption("--shard-id")
        selected = []
        deselected = []
        for item in items:
            bucket = selected if _shard_of(item.nodeid, count) == index else deselected
            bucket.append(item)

        if deselected:
            config.hook.pytest_deselected(items=deselected)
            items[:] = selected

    items[:] = order_k0s_last(items)


_QUARANTINES = pytest.StashKey[dict]()


def apply_quarantine(item) -> str | None:
    """Turn a ``quarantine(reason=...)`` marker into a non-strict, still-running xfail.

    A quarantined test stays visible: it runs every time, never fails the job,
    and surfaces as XPASS the moment the blocking bug is fixed. Returns the
    reason, or None when the item is not quarantined.
    """
    marker = item.get_closest_marker("quarantine")
    if marker is None:
        return None
    reason = marker.kwargs.get("reason")
    if not isinstance(reason, str) or not reason.strip():
        raise pytest.UsageError(
            f"{item.nodeid}: the quarantine marker requires a non-empty reason=... "
            "naming the product bug that blocks the test"
        )
    if item.get_closest_marker("xfail") is not None:
        raise pytest.UsageError(
            f"{item.nodeid}: carries both xfail and quarantine; drop the xfail, "
            "quarantine already implies a non-strict xfail"
        )
    reason = reason.strip()
    item.add_marker(pytest.mark.xfail(strict=False, run=True, reason=f"quarantined: {reason}"))
    return reason


# itemcollected fires per item during collection, before any modifyitems hook, so
# the conversion covers every shard's items and leaves the sharding and k0s-last
# ordering hook above untouched.
def pytest_itemcollected(item):
    reason = apply_quarantine(item)
    if reason is not None:
        item.config.stash.setdefault(_QUARANTINES, {})[item.nodeid] = reason


def pytest_terminal_summary(terminalreporter, config):
    quarantined = config.stash.get(_QUARANTINES, {})
    rows = [
        (outcome, report.nodeid, quarantined[report.nodeid])
        for outcome, key in (("XFAIL", "xfailed"), ("XPASS", "xpassed"))
        for report in terminalreporter.stats.get(key, [])
        if report.nodeid in quarantined
    ]
    if not rows:
        return
    terminalreporter.section("Quarantined")
    for outcome, nodeid, reason in sorted(rows, key=lambda row: (row[0] != "XPASS", row[1])):
        terminalreporter.write_line(f"{outcome} {nodeid}")
        terminalreporter.write_line(f"    {reason}")
        if outcome == "XPASS":
            terminalreporter.write_line(
                "    XPASS: consider removing the quarantine", yellow=True
            )


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
