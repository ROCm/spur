# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for the K8s failure diagnostics, run against a fake CoreV1 client."""

from datetime import datetime, timedelta, timezone
from types import SimpleNamespace as NS

import pytest

import k8s_diagnostics as diag
from k8s_diagnostics import (
    Budget,
    collect_namespace_diagnostics,
    failure_sections,
    format_events,
    format_pods,
    format_pvcs,
    register_namespaces,
    registered_namespaces,
    tail_lines,
    wants_diagnostics,
)

T0 = datetime(2026, 1, 1, 12, 0, 0, tzinfo=timezone.utc)


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


def pod(name, *, phase="Running", ready=True, restarts=0, waiting=None, node="w1", ip="10.0.0.1"):
    state = NS(waiting=NS(reason=waiting) if waiting else None)
    return NS(
        metadata=NS(name=name, deletion_timestamp=None),
        spec=NS(containers=[NS()], node_name=node),
        status=NS(
            phase=phase,
            pod_ip=ip,
            container_statuses=[NS(ready=ready, restart_count=restarts, state=state)],
        ),
    )


def pvc(name, phase="Pending", volume=None):
    return NS(
        metadata=NS(name=name),
        spec=NS(volume_name=volume, storage_class_name="local-path"),
        status=NS(phase=phase, capacity={"storage": "1Gi"} if volume else None),
    )


def event(reason, minutes, message="m", obj="spurctld-0"):
    return NS(
        metadata=NS(creation_timestamp=T0),
        last_timestamp=T0 + timedelta(minutes=minutes),
        event_time=None,
        type="Warning",
        reason=reason,
        message=message,
        involved_object=NS(kind="Pod", name=obj),
    )


class FakeCore:
    def __init__(self, pods=(), pvcs=(), events=(), logs=None, fail=()):
        self.pods = list(pods)
        self.pvcs = list(pvcs)
        self.events = list(events)
        self.logs = logs or {}
        self.fail = set(fail)
        self.log_calls = []

    def _maybe_fail(self, what):
        if what in self.fail:
            raise RuntimeError(f"{what} unavailable")

    def list_namespaced_pod(self, namespace, label_selector=None, **kwargs):
        self._maybe_fail("pods")
        label = (label_selector or "").removeprefix("app=")
        items = [p for p in self.pods if not label or p.metadata.name.startswith(label)]
        return NS(items=items)

    def list_namespaced_persistent_volume_claim(self, namespace, **kwargs):
        self._maybe_fail("pvc")
        return NS(items=self.pvcs)

    def list_namespaced_event(self, namespace, **kwargs):
        self._maybe_fail("events")
        return NS(items=self.events)

    def read_namespaced_pod_log(self, name, namespace, **kwargs):
        self.log_calls.append((name, kwargs))
        if name in self.fail:
            raise RuntimeError("log stream closed")
        return self.logs.get(name, "")


@pytest.mark.parametrize(
    "when,failed,expected",
    [
        ("setup", True, True),
        ("call", True, True),
        ("teardown", True, False),
        ("call", False, False),
        ("setup", False, False),
    ],
)
def test_only_failed_setup_and_call_attach_diagnostics(when, failed, expected):
    assert wants_diagnostics(when, failed) is expected


def test_tail_lines_keeps_the_last_lines():
    assert tail_lines("a\nb\nc\nd", 2) == "c\nd"
    assert tail_lines("a\nb", 5) == "a\nb"


def test_format_pods_shows_waiting_reason_ready_restarts_and_node():
    text = format_pods([pod("spurctld-0", ready=False, restarts=3, waiting="CrashLoopBackOff", node="w2")])
    header, row = text.splitlines()
    assert header.split() == ["NAME", "READY", "STATUS", "RESTARTS", "IP", "NODE"]
    assert row.split() == ["spurctld-0", "0/1", "CrashLoopBackOff", "3", "10.0.0.1", "w2"]


def test_format_pods_marks_unscheduled_pod_without_node():
    text = format_pods([pod("p", phase="Pending", ready=False, node=None, ip=None)])
    assert text.splitlines()[1].split() == ["p", "0/1", "Pending", "0", "<none>", "<none>"]


def test_format_pvcs_shows_pending_claim_without_volume():
    text = format_pvcs([pvc("spool-spurctld-0"), pvc("spool-spurctld-1", "Bound", "pv-1")])
    rows = [line.split() for line in text.splitlines()[1:]]
    assert rows[0] == ["spool-spurctld-0", "Pending", "<none>", "<none>", "local-path"]
    assert rows[1] == ["spool-spurctld-1", "Bound", "pv-1", "1Gi", "local-path"]


def test_format_events_sorts_by_time_and_keeps_only_the_most_recent():
    events = [event(f"R{i}", i) for i in (5, 1, 4, 2, 3)]
    reasons = [line.split()[2] for line in format_events(events, limit=3).splitlines()[1:]]
    assert reasons == ["R3", "R4", "R5"]


def test_format_events_falls_back_to_creation_time_and_flattens_messages():
    bare = event("Late", 0, message="line one\nline two")
    bare.last_timestamp = None
    text = format_events([bare])
    assert "12:00:00" in text
    assert "line one line two" in text


def test_budget_runs_out_and_caps_each_call_timeout():
    clock = FakeClock()
    budget = Budget(seconds=12, clock=clock)
    assert budget.call_timeout() == diag.CALL_TIMEOUT_S
    clock.now = 5
    assert budget.call_timeout() == 7
    clock.now = 12
    assert budget.exhausted()
    assert budget.call_timeout() == 0


def test_namespace_diagnostics_cover_pods_pvcs_events_and_both_log_tails():
    core = FakeCore(
        pods=[pod("spurctld-0"), pod("spur-k8s-operator-abc")],
        pvcs=[pvc("spool-spurctld-0")],
        events=[event("FailedScheduling", 1)],
        logs={"spurctld-0": "ctld line", "spur-k8s-operator-abc": "op line"},
    )
    sections = dict(collect_namespace_diagnostics(core, "ns1", Budget()))
    assert list(sections) == [
        "diagnostics: pods (ns1)",
        "diagnostics: pvc (ns1)",
        "diagnostics: events (ns1)",
        "diagnostics: spurctld logs (ns1)",
        "diagnostics: operator logs (ns1)",
    ]
    assert "spool-spurctld-0" in sections["diagnostics: pvc (ns1)"]
    assert "FailedScheduling" in sections["diagnostics: events (ns1)"]
    assert "ctld line" in sections["diagnostics: spurctld logs (ns1)"]
    assert "op line" in sections["diagnostics: operator logs (ns1)"]


def test_log_tail_is_limited_to_the_configured_line_count():
    core = FakeCore(pods=[pod("spurctld-0")], logs={"spurctld-0": "\n".join(f"l{i}" for i in range(500))})
    sections = dict(collect_namespace_diagnostics(core, "ns", Budget()))
    lines = sections["diagnostics: spurctld logs (ns)"].splitlines()
    assert lines[0] == "=== spurctld-0 ==="
    assert len(lines) - 1 == diag.LOG_LINES
    assert lines[-1] == "l499"
    assert core.log_calls[0][1]["tail_lines"] == diag.LOG_LINES


def test_one_failing_collector_does_not_blank_the_others():
    core = FakeCore(pods=[pod("spurctld-0")], events=[event("E", 1)], fail={"pvc", "spurctld-0"})
    sections = dict(collect_namespace_diagnostics(core, "ns", Budget()))
    assert sections["diagnostics: pvc (ns)"] == "collection failed: RuntimeError: pvc unavailable"
    assert "E" in sections["diagnostics: events (ns)"]
    assert "log read failed: RuntimeError: log stream closed" in sections["diagnostics: spurctld logs (ns)"]


def test_pod_listing_failure_is_reported_for_pods_and_log_sections():
    core = FakeCore(fail={"pods"})
    sections = dict(collect_namespace_diagnostics(core, "ns", Budget()))
    assert sections["diagnostics: pods (ns)"].startswith("collection failed:")
    assert sections["diagnostics: operator logs (ns)"].startswith("collection failed:")


def test_exhausted_budget_skips_every_section_without_calling_the_cluster():
    clock = FakeClock()
    budget = Budget(seconds=1, clock=clock)
    clock.now = 2
    core = FakeCore(pods=[pod("spurctld-0")], fail={"pods", "pvc", "events"})
    sections = collect_namespace_diagnostics(core, "ns", budget)
    assert len(sections) == 5
    assert all("budget exhausted" in text for _, text in sections)


def test_failure_sections_is_empty_for_a_passed_or_teardown_phase():
    assert failure_sections("call", False, ["ns"], FakeCore) == []
    assert failure_sections("teardown", True, ["ns"], FakeCore) == []


def test_failure_sections_covers_every_registered_namespace():
    sections = failure_sections("call", True, ["a", "b"], FakeCore)
    titles = [title for title, _ in sections]
    assert "diagnostics: pods (a)" in titles
    assert "diagnostics: operator logs (b)" in titles


def test_failure_sections_reports_a_missing_client_instead_of_raising():
    def broken_factory():
        raise RuntimeError("no kubeconfig")

    [(title, text)] = failure_sections("setup", True, ["ns"], broken_factory)
    assert title == "diagnostics"
    assert "no kubeconfig" in text


def test_failure_sections_says_so_when_no_namespace_exists_yet():
    [(_, text)] = failure_sections("setup", True, [], FakeCore)
    assert "no test namespace" in text


def test_register_namespaces_keeps_each_name_once(monkeypatch):
    monkeypatch.setattr(diag, "_namespaces", [])
    register_namespaces("a", "b")
    register_namespaces("a")
    assert registered_namespaces() == ["a", "b"]
