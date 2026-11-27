# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Cluster state attached to a failing K8s E2E test.

The CI diagnostics artifact expires and is not in the pytest output, so the
same state is collected here and put in the failing test's report.
"""

from __future__ import annotations

import time
from datetime import datetime, timezone

DIAGNOSTICS_BUDGET_S = 60.0
CALL_TIMEOUT_S = 10.0
EVENT_LIMIT = 50
LOG_LINES = 200
LOG_SELECTORS = (("spurctld", "app=spurctld"), ("operator", "app=spur-k8s-operator"))

_EPOCH = datetime.min.replace(tzinfo=timezone.utc)

# Namespaces of the running suite, so the failure hook can find them without every
# fixture having to hand them over.
_namespaces: list[str] = []


def register_namespaces(*names: str) -> None:
    for name in names:
        if name not in _namespaces:
            _namespaces.append(name)


def registered_namespaces() -> list[str]:
    return list(_namespaces)


def wants_diagnostics(when: str, failed: bool) -> bool:
    """Only a failed setup or call; skips, xfails and teardown failures stay quiet."""
    return failed and when in ("setup", "call")


class Budget:
    """Wall-clock allowance for everything one failing test collects."""

    def __init__(self, seconds: float = DIAGNOSTICS_BUDGET_S, clock=time.monotonic):
        self._clock = clock
        self._deadline = clock() + seconds

    def remaining(self) -> float:
        return max(0.0, self._deadline - self._clock())

    def exhausted(self) -> bool:
        return self.remaining() <= 0

    def call_timeout(self) -> float:
        return min(CALL_TIMEOUT_S, self.remaining())


def tail_lines(text: str, limit: int) -> str:
    return "\n".join(text.splitlines()[-limit:])


def format_table(headers: list[str], rows: list[list[str]]) -> str:
    table = [headers] + rows
    widths = [max(len(row[i]) for row in table) for i in range(len(headers))]
    return "\n".join("  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip() for row in table)


def _ready(pod) -> str:
    statuses = pod.status.container_statuses or []
    return f"{sum(1 for s in statuses if s.ready)}/{len(pod.spec.containers)}"


def _restarts(pod) -> str:
    return str(sum(s.restart_count for s in pod.status.container_statuses or []))


def _pod_status(pod) -> str:
    for status in pod.status.container_statuses or []:
        waiting = status.state.waiting if status.state else None
        if waiting is not None and waiting.reason:
            return waiting.reason
    if pod.metadata.deletion_timestamp is not None:
        return "Terminating"
    return pod.status.phase or "Unknown"


def format_pods(pods) -> str:
    rows = [
        [
            pod.metadata.name,
            _ready(pod),
            _pod_status(pod),
            _restarts(pod),
            pod.status.pod_ip or "<none>",
            pod.spec.node_name or "<none>",
        ]
        for pod in pods
    ]
    return format_table(["NAME", "READY", "STATUS", "RESTARTS", "IP", "NODE"], rows)


def format_pvcs(pvcs) -> str:
    rows = [
        [
            pvc.metadata.name,
            pvc.status.phase or "Unknown",
            pvc.spec.volume_name or "<none>",
            (pvc.status.capacity or {}).get("storage", "<none>"),
            pvc.spec.storage_class_name or "<none>",
        ]
        for pvc in pvcs
    ]
    return format_table(["NAME", "STATUS", "VOLUME", "CAPACITY", "STORAGECLASS"], rows)


def _event_time(event) -> datetime:
    return event.last_timestamp or event.event_time or event.metadata.creation_timestamp or _EPOCH


def format_events(events, limit: int = EVENT_LIMIT) -> str:
    """The *limit* most recent events, oldest first."""
    recent = sorted(events, key=_event_time)[-limit:]
    rows = [
        [
            _event_time(event).strftime("%H:%M:%S") if _event_time(event) != _EPOCH else "<unknown>",
            event.type or "",
            event.reason or "",
            f"{event.involved_object.kind}/{event.involved_object.name}",
            (event.message or "").replace("\n", " "),
        ]
        for event in recent
    ]
    return format_table(["TIME", "TYPE", "REASON", "OBJECT", "MESSAGE"], rows)


def _section(title: str, budget: Budget, collect) -> tuple[str, str]:
    """Runs on a test that is already failing, so any trouble is reported as text."""
    if budget.exhausted():
        return title, "skipped (diagnostics budget exhausted)"
    try:
        return title, collect(budget.call_timeout())
    except Exception as err:
        return title, f"collection failed: {type(err).__name__}: {err}"


def collect_namespace_diagnostics(core_v1, namespace: str, budget: Budget) -> list[tuple[str, str]]:
    """Pods, PVCs, recent events and controller/operator log tails of *namespace*.
    A section that cannot be collected says why in its own text; this never raises."""
    sections = [
        _section(
            f"diagnostics: pods ({namespace})",
            budget,
            lambda t: format_pods(core_v1.list_namespaced_pod(namespace, _request_timeout=t).items),
        ),
        _section(
            f"diagnostics: pvc ({namespace})",
            budget,
            lambda t: format_pvcs(core_v1.list_namespaced_persistent_volume_claim(namespace, _request_timeout=t).items),
        ),
        _section(
            f"diagnostics: events ({namespace})",
            budget,
            lambda t: format_events(core_v1.list_namespaced_event(namespace, _request_timeout=t).items),
        ),
    ]
    for name, selector in LOG_SELECTORS:
        sections.append(_section(
            f"diagnostics: {name} logs ({namespace})",
            budget,
            lambda t, selector=selector: _pod_logs(core_v1, namespace, selector, t, budget),
        ))
    return sections


def _pod_logs(core_v1, namespace: str, selector: str, timeout: float, budget: Budget) -> str:
    pods = core_v1.list_namespaced_pod(namespace, label_selector=selector, _request_timeout=timeout).items
    if not pods:
        return "no pods"
    parts = []
    for pod in pods:
        name = pod.metadata.name
        if budget.exhausted():
            parts.append(f"=== {name} ===\nskipped (diagnostics budget exhausted)")
            continue
        try:
            log = core_v1.read_namespaced_pod_log(
                name, namespace, tail_lines=LOG_LINES, _request_timeout=budget.call_timeout()
            )
        except Exception as err:
            log = f"log read failed: {type(err).__name__}: {err}"
        parts.append(f"=== {name} ===\n{tail_lines(log, LOG_LINES)}")
    return "\n".join(parts)


def failure_sections(when: str, failed: bool, namespaces: list[str], core_v1_factory,
                     budget: Budget | None = None) -> list[tuple[str, str]]:
    """Report sections for a test phase; empty unless a setup or call failed."""
    if not wants_diagnostics(when, failed):
        return []
    budget = budget or Budget()
    if not namespaces:
        return [("diagnostics", "no test namespace registered yet")]
    try:
        core_v1 = core_v1_factory()
    except Exception as err:
        return [("diagnostics", f"no cluster client: {type(err).__name__}: {err}")]
    return [section for namespace in namespaces
            for section in collect_namespace_diagnostics(core_v1, namespace, budget)]
