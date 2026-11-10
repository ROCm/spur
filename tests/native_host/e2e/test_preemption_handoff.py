# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Exercise resource handoffs through real eviction, epilog, and placement."""

import json
import re
import shlex
import time

import pytest

from cluster import SpurCluster, make_remote_dir, parse_job_id, wait_job, wait_job_state, wait_until


@pytest.fixture
def handoff_cluster(ssh_nodes, remote_bin_dir):
    yield from _handoff_cluster(ssh_nodes, remote_bin_dir, 3)


@pytest.fixture
def wide_handoff_cluster(ssh_nodes, remote_bin_dir):
    yield from _handoff_cluster(ssh_nodes, remote_bin_dir, 4)


@pytest.fixture
def rollup_cluster(ssh_nodes, remote_bin_dir):
    yield from _handoff_cluster(ssh_nodes, remote_bin_dir, 3, retention=0)


def _handoff_cluster(ssh_nodes, remote_bin_dir, count, retention=86400):
    if len(ssh_nodes) < count:
        pytest.skip(f"this handoff test requires {count} nodes")
    c = SpurCluster(ssh_nodes[:count], make_remote_dir(), remote_bin_dir)
    try:
        c.enable_accounting()
    except RuntimeError as exc:
        pytest.skip(str(exc))
    try:
        c.provision()
        epilog = c.write_file(
            "handoff-epilog.sh",
            '#!/bin/bash\n'
            f'gate={shlex.quote(c.remote_dir)}/hold-$SPUR_JOB_ID\n'
            'touch "$gate.entered"\n'
            # A gate holding a deadline (epoch ns) opens itself, so nodes can release together.
            'while [ -e "$gate" ]; do\n'
            '  at=$(cat "$gate" 2>/dev/null)\n'
            '  [ -n "$at" ] && [ "$(date +%s%N)" -ge "$at" ] && break\n'
            '  sleep 0.02\n'
            'done\n',
            all_nodes=True,
        )
        prolog = c.write_file(
            "handoff-prolog.sh",
            '#!/bin/bash\n'
            f'gate={shlex.quote(c.remote_dir)}/launch-$SPUR_JOB_ID\n'
            'touch "$gate.entered"\n'
            'while [ -e "$gate" ]; do sleep 0.02; done\n'
            '[ ! -e "$gate.fail" ]\n',
            all_nodes=True,
        )
        # A broad stale-daemon sweep would also stop unrelated Spur daemons sharing these hosts.
        c.start({
            "controller": {"terminal_job_retention_secs": retention},
            "scheduler": {"preempt_type": "qos_priority", "complete_wait_secs": 300},
            "hooks": {"epilog": epilog, "prolog": prolog},
            "auth": {"allow_root_jobs": True},
        }, kill_stale=False)
        for name, priority, targets, mode in [
            ("victim-a", 100, "", "cancel"),
            ("victim-b", 100, "", "cancel"),
            ("victim-r", 100, "", "requeue"),
            ("hunter-a", 10000, "victim-a,victim-r", "off"),
            ("hunter-b", 5000, "victim-b", "off"),
            ("bystander", 20000, "", "off"),
            ("plain", 100, "", "off"),
        ]:
            args = ["add", "qos", f"name={name}", f"priority={priority}",
                    f"preemptmode={mode}"]
            if targets:
                args.append(f"preempt={targets}")
            c.cli(["spur", "sacctmgr"] + args)
        # The controller's accounting caches refresh every ten seconds in this fixture.
        time.sleep(15)
        yield c
    finally:
        for node in c.nodes:
            node.exec_allow_fail(
                f"rm -f {shlex.quote(c.remote_dir)}/hold-* {shlex.quote(c.remote_dir)}/launch-*"
            )
        c.teardown()


def _submit(c, name, qos, nodes=None, count=1, limit="10:00", sleep=600, extra=()):
    script = c.write_file(f"{name}.sh", f"#!/bin/bash\nsleep {sleep}\n", all_nodes=True)
    args = ["--exclusive", f"-N{count}", "-t", limit, "-J", name, "-q", qos, *extra]
    if nodes:
        args.append(f"--nodelist={nodes}")
    jid = parse_job_id(c.sbatch(args + [script]))
    assert jid is not None
    return jid


def _gate(c, job, indices, hold):
    path = shlex.quote(f"{c.remote_dir}/hold-{job}")
    for index in indices:
        c.nodes[index].exec(f"{'touch' if hold else 'rm -f'} {path}")


def _wait_epilog(c, job, indices):
    deadline = time.monotonic() + 60
    path = shlex.quote(f"{c.remote_dir}/hold-{job}.entered")
    while time.monotonic() < deadline:
        if all(c.nodes[i].exec_allow_fail(f"test -e {path} && echo ready").strip() == "ready"
               for i in indices):
            return
        time.sleep(0.2)
    pytest.fail(f"victim {job} never reached epilog on all expected nodes")


def _state(c, job, state):
    output = c.scontrol("show", "job", str(job))
    assert f"JobState={state}" in output, output
    return output


def _cycles(c):
    output = c.sdiag()
    match = re.search(r"Cycles\s+:\s+(\d+)", output)
    assert match, output
    return int(match.group(1))


def _assert_pending_for_passes(c, jobs, passes=4):
    target = _cycles(c) + passes
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        for job in jobs:
            _state(c, job, "PENDING")
        if _cycles(c) >= target:
            return
        time.sleep(0.2)
    pytest.fail("scheduler did not finish the observed placement passes")


def _competing_preemptors(c):
    n1, n2, n3 = c.node_names
    a = _submit(c, "victim-a", "victim-a", n1)
    b = _submit(c, "victim-b", "victim-b", f"{n2},{n3}", 2)
    for jid in (a, b):
        wait_job_state(c, jid, "R", timeout=60)
    _gate(c, a, [0], True)
    _gate(c, b, [1, 2], True)
    pinned = _submit(c, "pinned", "hunter-b", f"{n2},{n3}", 2)
    _wait_epilog(c, b, [1, 2])
    flexible = _submit(c, "flexible", "hunter-a")
    _wait_epilog(c, a, [0])
    for victim, owner in [(a, flexible), (b, pinned)]:
        show = _state(c, victim, "COMPLETING")
        assert f"PreemptedBy={owner}" in show, show
    _gate(c, b, [1], False)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        _state(c, flexible, "PENDING")
        _state(c, pinned, "PENDING")
        show = c.scontrol("show", "node", n2)
        if re.search(r"\bCPUAlloc=0\b", show):
            _state(c, b, "COMPLETING")
            return a, b, flexible, pinned
        time.sleep(0.2)
    pytest.fail("the early victim node was never released")


def test_competing_preemptors_keep_staggered_nodes(handoff_cluster):
    c = handoff_cluster
    a, b, flexible, pinned = _competing_preemptors(c)
    _assert_pending_for_passes(c, [flexible, pinned])
    _gate(c, b, [2], False)
    wait_job_state(c, pinned, "R", timeout=60)
    _state(c, flexible, "PENDING")
    show = _state(c, pinned, "RUNNING")
    assert "NumNodes=2" in show, show
    _gate(c, a, [0], False)
    wait_job_state(c, flexible, "R", timeout=60)
    show = _state(c, flexible, "RUNNING")
    assert f"NodeList={c.node_names[0]}" in show, show


@pytest.mark.parametrize("action", ["cancel", "hold"])
def test_abandoned_handoff_reuses_freed_node(handoff_cluster, action):
    c = handoff_cluster
    _, _, flexible, pinned = _competing_preemptors(c)
    _assert_pending_for_passes(c, [flexible, pinned])
    if action == "cancel":
        c.cli(["scancel", str(pinned)])
    else:
        c.scontrol("hold", str(pinned))
    wait_job_state(c, flexible, "R", timeout=60)
    show = _state(c, flexible, "RUNNING")
    assert f"NodeList={c.node_names[1]}" in show, show


def test_requeued_victim_node_goes_to_preemptor_over_higher_priority_bystander(handoff_cluster):
    c = handoff_cluster
    node = c.node_names[0]
    victim = _submit(c, "victim", "victim-r", node)
    wait_job_state(c, victim, "R", timeout=60)
    bystander = _submit(c, "bystander", "bystander", node)
    hunter = _submit(c, "hunter", "hunter-a", node)
    wait_job_state(c, hunter, "R", timeout=60)
    show = c.scontrol("show", "job", str(victim))
    assert f"PreemptedBy={hunter}" in show and "PreemptMode=Requeue" in show, show
    _state(c, bystander, "PENDING")
    show = _state(c, hunter, "RUNNING")
    assert f"NodeList={node}" in show, show


def test_wide_job_behind_handoff_owner_keeps_its_planned_slot(handoff_cluster):
    c = handoff_cluster
    n1, n2, _ = c.node_names
    victim = _submit(c, "victim", "victim-a", n1)
    filler = _submit(c, "filler", "plain", n2)
    for jid in (victim, filler):
        wait_job_state(c, jid, "R", timeout=60)
    _gate(c, victim, [0], True)
    owner = _submit(c, "owner", "hunter-a", n1, limit="1:00", sleep=20)
    _wait_epilog(c, victim, [0])
    show = _state(c, victim, "COMPLETING")
    assert f"PreemptedBy={owner}" in show, show
    wide = _submit(c, "wide", "hunter-b", f"{n1},{n2}", 2)
    narrow = _submit(c, "narrow", "plain", n2, limit="30:00")
    c.cli(["scancel", str(filler)])
    wait_job(c, filler, timeout=60)
    # The owner still waits on its victim; the narrow job must not jump the wide job's plan.
    _assert_pending_for_passes(c, [owner, wide, narrow])
    _gate(c, victim, [0], False)
    wait_job_state(c, owner, "R", timeout=60)
    assert f"NodeList={n1}" in _state(c, owner, "RUNNING")
    deadline = time.monotonic() + 120
    while "JobState=RUNNING" not in c.scontrol("show", "job", str(wide)):
        _state(c, narrow, "PENDING")
        assert time.monotonic() < deadline, "wide job never started after the owner"
        time.sleep(0.5)
    assert "NumNodes=2" in _state(c, wide, "RUNNING")
    _state(c, narrow, "PENDING")


def _preempted_by(c, job):
    match = re.search(r"PreemptedBy=(\d+)", c.scontrol("show", "job", str(job)))
    return int(match.group(1)) if match else None


def test_wide_preemptor_uses_the_node_an_owner_left_unused(wide_handoff_cluster):
    c = wide_handoff_cluster
    n1, n2, n3, n4 = c.node_names
    victim = _submit(c, "victim", "victim-a", f"{n1},{n2}", 2)
    low = [_submit(c, f"low{i}", "victim-b", node) for i, node in ((3, n3), (4, n4))]
    for jid in (victim, *low):
        wait_job_state(c, jid, "R", timeout=60)
    _gate(c, victim, [0, 1], True)
    owner = _submit(c, "owner", "hunter-a")
    _wait_epilog(c, victim, [0, 1])
    assert f"PreemptedBy={owner}" in _state(c, victim, "COMPLETING")
    # Eligible only once the victim ends, so it first searches in the pass the owner is placed.
    wide = _submit(c, "wide", "hunter-b", count=2, extra=[f"--dependency=afterany:{victim}"])
    _assert_pending_for_passes(c, [wide])
    at = time.time_ns() + 2_000_000_000
    path = shlex.quote(f"{c.remote_dir}/hold-{victim}")
    for index in (0, 1):
        c.nodes[index].exec(f"echo {at} > {path}")
    wait_job_state(c, owner, "R", timeout=60)
    wait_job_state(c, wide, "R", timeout=120)
    evicted = [jid for jid in low if _preempted_by(c, jid) == wide]
    assert len(evicted) == 1, f"wide job evicted {evicted} of {low}"
    show = _state(c, wide, "RUNNING")
    assert "NumNodes=2" in show, show


def test_preemptor_counts_the_node_a_completing_job_already_released(handoff_cluster):
    c = handoff_cluster
    n1, n2, n3 = c.node_names
    done = _submit(c, "done", "plain", f"{n1},{n3}", 2)
    low = _submit(c, "low", "victim-b", n2)
    for jid in (done, low):
        wait_job_state(c, jid, "R", timeout=60)
    _gate(c, done, [2], True)
    c.cli(["scancel", str(done)])
    _wait_epilog(c, done, [2])
    deadline = time.monotonic() + 60
    while not re.search(r"\bCPUAlloc=0\b", c.scontrol("show", "node", n1)):
        assert time.monotonic() < deadline, "the completing job never released its first node"
        time.sleep(0.2)
    hunter = _submit(c, "hunter", "hunter-b", count=2)
    wait_job_state(c, hunter, "R", timeout=60)
    _state(c, done, "COMPLETING")
    assert _preempted_by(c, low) == hunter


def test_preemptor_does_not_evict_for_a_node_assigned_in_the_same_pass(handoff_cluster):
    c = handoff_cluster
    n1, n2, n3 = c.node_names
    low = _submit(c, "low", "victim-b", n2)
    other = _submit(c, "other", "plain", n3)
    for jid in (low, other):
        wait_job_state(c, jid, "R", timeout=60)
    # A shared begin time makes both eligible in one pass; the hunter sorts first by priority.
    begin = f"--begin={time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(time.time() + 8))}"
    hunter = _submit(c, "hunter", "hunter-b", f"{n1},{n2}", 2, extra=[begin])
    short = _submit(c, "short", "plain", n1, limit="1:00", sleep=20, extra=[begin])
    wait_job_state(c, short, "R", timeout=60)
    _assert_pending_for_passes(c, [hunter])
    assert _preempted_by(c, low) is None
    _state(c, low, "RUNNING")
    wait_job(c, short, timeout=60)
    wait_job_state(c, hunter, "R", timeout=60)
    assert _preempted_by(c, low) == hunter


def _usage(c, *, used=0, open_count=0, unused=None, jobs=None):
    unused = unused or {}
    totals = (used, sum(unused.values()), open_count)
    expected_series = {
        (reason, initiator): unused.get((reason, initiator), 0)
        for reason in ("started_elsewhere", "launch_failed", "cancelled", "ended_other")
        for initiator in ("user", "admin", "system", "unknown")
    }

    def check():
        body = c.nodes[0].exec("curl -fsS http://127.0.0.1:6822/metrics/scheduler")
        prefix = "spur_scheduler_preemptions_"
        for family, kind in (("used", "counter"), ("unused", "counter"), ("open", "gauge")):
            assert f"# TYPE {prefix}{family} {kind}\n" in body, body
        for name, value in (("used_total", used), ("open", open_count)):
            assert re.search(rf"^{prefix}{name} {value}$", body, re.M), body
        series = {}
        for line in body.splitlines():
            if line.startswith(f"{prefix}unused_total"):
                match = re.fullmatch(rf'{prefix}unused_total\{{([^}}]+)\}} (\d+)', line)
                assert match, line
                labels = dict(re.findall(r'(\w+)="([^"]*)"', match[1]))
                assert set(labels) == {"reason", "initiator"}, labels
                key = (labels["reason"], labels["initiator"])
                assert key not in series, line
                series[key] = int(match[2])
        assert series == expected_series, series

        diag = c.sdiag()
        match = re.search(r"Preemptions \(lifetime\): used=(\d+) unused=(\d+) open=(\d+)", diag)
        assert match and tuple(map(int, match.groups())) == totals, diag
        causes = {
            (reason, initiator): int(count)
            for reason, initiator, count in re.findall(
                r"Unused: reason=(\w+) initiator=(\w+) count=(\d+)", diag
            )
        }
        assert causes == unused, diag
        assert not re.search(r"User .*: used=", diag), diag
        for job, (job_used, job_open, job_unused) in (jobs or {}).items():
            show = c.scontrol("show", "job", str(job))
            match = re.search(r"PreemptionsUsed=(\d+) PreemptionsUnused=(\d+) PreemptionsOpen=(\d+)", show)
            assert match and tuple(map(int, match.groups())) == (
                job_used, sum(job_unused.values()), job_open
            ), show
            causes = {
                (reason, initiator): int(count)
                for reason, initiator, count in re.findall(
                    r"UnusedPreemptionReason=(\w+) Initiator=(\w+) Count=(\d+)", show
                )
            }
            assert causes == job_unused, show
        return True

    wait_until(check, timeout_s=60, interval_s=0.2,
               desc=f"preemption usage {totals}, causes={unused}, jobs={jobs}",
               tolerate=(AssertionError,))


def _launch_gate(c, job, indices, hold):
    path = shlex.quote(f"{c.remote_dir}/launch-{job}")
    for index in indices:
        c.nodes[index].exec(f"{'touch' if hold else 'rm -f'} {path}")


def _wait_prolog(c, job, indices):
    path = shlex.quote(f"{c.remote_dir}/launch-{job}.entered")
    wait_until(
        lambda: all(c.nodes[i].exec_allow_fail(f"test -e {path} && echo ready").strip() == "ready"
                    for i in indices),
        timeout_s=60, interval_s=0.2, desc=f"job {job} waiting inside node prolog",
    )


def _accounted_usage(c, job, *, used=0, unused=None):
    expected = {"used": used, "open": 0, "unused": unused or {}}

    def check():
        raw = c.psql(f"SELECT preemption_usage FROM jobs WHERE job_id = {int(job)}")
        assert raw.strip(), f"job {job} has no accounting row"
        stored = json.loads(raw)
        assert {key: stored.get(key) for key in expected} == expected, raw
        return True

    # Late settlements repair existing rows on the two-minute reconciliation pass.
    wait_until(check, timeout_s=150, interval_s=1,
               desc=f"accounting usage for job {job}: {expected}", tolerate=(AssertionError,))


def test_usage_counts_multinode_victim_once_after_launch_confirmation(handoff_cluster):
    c = handoff_cluster
    nodes = ",".join(c.node_names[:2])
    _usage(c)
    victim = _submit(c, "usage-victim", "victim-a", nodes, 2)
    wait_job_state(c, victim, "R", timeout=60)
    _gate(c, victim, [0, 1], True)
    hunter = _submit(c, "usage-hunter", "hunter-a", nodes, 2)
    _wait_epilog(c, victim, [0, 1])
    _launch_gate(c, hunter, [0, 1], True)
    _usage(c, jobs={hunter: (0, 0, {}), victim: (0, 0, {})})

    _gate(c, victim, [0], False)
    _usage(c, open_count=1, jobs={hunter: (0, 1, {})})
    _state(c, hunter, "PENDING")
    c.restart_controller()
    _usage(c, open_count=1, jobs={hunter: (0, 1, {})})

    _gate(c, victim, [1], False)
    _wait_prolog(c, hunter, [0, 1])
    _state(c, hunter, "PENDING")
    _usage(c, open_count=1, jobs={hunter: (0, 1, {})})
    _launch_gate(c, hunter, [0, 1], False)
    _usage(c, used=1, jobs={hunter: (1, 0, {}), victim: (0, 0, {})})

    c.cli(["scancel", str(hunter)])
    assert wait_job(c, hunter, timeout=60) == "CA"
    _usage(c, used=1, jobs={hunter: (1, 0, {})})
    _accounted_usage(c, hunter, used=1)
    c.cli(["spur", "sdiag", "--reset"])
    _usage(c, used=1, jobs={hunter: (1, 0, {})})
    c.restart_controller()
    _usage(c, used=1, jobs={hunter: (1, 0, {})})


def test_usage_cancelled_request_counts_each_victim_including_late_release(handoff_cluster):
    c = handoff_cluster
    victims = [_submit(c, f"usage-victim-{i}", "victim-a", c.node_names[i]) for i in (0, 1)]
    for i, victim in enumerate(victims):
        wait_job_state(c, victim, "R", timeout=60)
        _gate(c, victim, [i], True)
    hunter = _submit(c, "usage-hunter", "hunter-a", ",".join(c.node_names[:2]), 2)
    for i, victim in enumerate(victims):
        _wait_epilog(c, victim, [i])
        assert f"PreemptedBy={hunter}" in _state(c, victim, "COMPLETING")
    _usage(c, jobs={hunter: (0, 0, {})})
    _gate(c, victims[0], [0], False)
    _usage(c, open_count=1, jobs={hunter: (0, 1, {})})
    c.cli(["scancel", str(hunter)])
    assert wait_job(c, hunter, timeout=60) == "CA"
    cause = {("cancelled", "user"): 1}
    _usage(c, unused=cause, jobs={hunter: (0, 0, cause)})
    _accounted_usage(c, hunter, unused={"cancelled": {"user": 1}})
    c.restart_controller()
    _usage(c, unused=cause, jobs={hunter: (0, 0, cause)})
    _gate(c, victims[1], [1], False)
    cause = {("cancelled", "user"): 2}
    _usage(c, unused=cause, jobs={hunter: (0, 0, cause)})
    _accounted_usage(c, hunter, unused={"cancelled": {"user": 2}})
    _scenario_report(c, unused={("cancelled", "user"): 2})


def test_usage_started_elsewhere_and_cancelled_have_distinct_causes(handoff_cluster):
    c = handoff_cluster
    a, b, flexible, pinned = _competing_preemptors(c)
    _usage(c, open_count=1, jobs={pinned: (0, 1, {}), flexible: (0, 0, {})})
    c.cli(["scancel", str(pinned)])
    wait_job_state(c, flexible, "R", timeout=60)
    assert f"NodeList={c.node_names[1]}" in _state(c, flexible, "RUNNING")
    cancelled = {("cancelled", "user"): 1}
    _usage(c, unused=cancelled, jobs={pinned: (0, 0, cancelled), flexible: (0, 0, {})})
    _gate(c, a, [0], False)
    _gate(c, b, [2], False)
    elsewhere = {("started_elsewhere", "system"): 1}
    _usage(c, unused={**cancelled, **elsewhere},
           jobs={pinned: (0, 0, cancelled), flexible: (0, 0, elsewhere)})
    c.cli(["scancel", str(flexible)])
    assert wait_job(c, flexible, timeout=60) == "CA"
    _usage(c, unused={**cancelled, **elsewhere})
    _accounted_usage(c, flexible, unused={"started_elsewhere": {"system": 1}})
    _scenario_report(c, unused={**cancelled, **elsewhere})


@pytest.mark.parametrize("failure", ["launch", "application"])
def test_usage_distinguishes_launch_failure_from_later_application_failure(handoff_cluster, failure):
    c = handoff_cluster
    victim = _submit(c, "usage-victim", "victim-a", c.node_names[0])
    wait_job_state(c, victim, "R", timeout=60)
    _gate(c, victim, [0], True)
    finish = f"{c.remote_dir}/finish-application"
    script = c.write_file(
        "usage-failure.sh",
        f"#!/bin/bash\nwhile [ ! -e {shlex.quote(finish)} ]; do sleep 0.02; done\nexit 7\n",
        all_nodes=True,
    )
    hunter = parse_job_id(c.sbatch([
        "--exclusive", "-N1", "-t", "10:00", "-q", "hunter-a",
        f"--nodelist={c.node_names[0]}", script,
    ]))
    assert hunter is not None
    _wait_epilog(c, victim, [0])
    _launch_gate(c, hunter, [0], True)
    if failure == "launch":
        c.nodes[0].exec(f"touch {shlex.quote(f'{c.remote_dir}/launch-{hunter}.fail')}")
    _gate(c, victim, [0], False)
    _wait_prolog(c, hunter, [0])
    _state(c, hunter, "PENDING")
    _usage(c, open_count=1, jobs={hunter: (0, 1, {})})
    _launch_gate(c, hunter, [0], False)
    if failure == "launch":
        cause = {("launch_failed", "system"): 1}
        _usage(c, unused=cause, jobs={hunter: (0, 0, cause)})
        c.cli(["scancel", str(hunter)])
        wait_job(c, hunter, timeout=60)
        _usage(c, unused=cause, jobs={hunter: (0, 0, cause)})
        _accounted_usage(c, hunter, unused={"launch_failed": {"system": 1}})
        _scenario_report(c, unused=cause)
    else:
        _usage(c, used=1, jobs={hunter: (1, 0, {})})
        c.nodes[0].exec(f"touch {shlex.quote(finish)}")
        assert wait_job(c, hunter, timeout=60) == "F"
        assert "ExitCode=7:0" in _state(c, hunter, "FAILED")
        _usage(c, used=1, jobs={hunter: (1, 0, {})})
        _accounted_usage(c, hunter, used=1)
        _scenario_report(c, used=1)


def _rollup_report(c, expected, *filters, causes=None):
    def check():
        text = c.cli(["spur", "sreport", "preemptions", "--parsable", "--noheader",
                      *filters])
        rows = [line.split("|") for line in text.splitlines() if "|" in line]
        assert all(len(row) == 5 for row in rows), text
        actual = {(row[0], row[1]): tuple(map(int, row[2:5])) for row in rows}
        assert len(actual) == len(rows) and actual == expected, text
        if causes is not None:
            text = c.cli(["spur", "sreport", "preemptions", "--parsable", "--noheader",
                          "--by-cause", *filters])
            details = [line.split("|") for line in text.splitlines() if "|" in line]
            assert all(len(row) == 5 and row[2] and row[3] for row in details), text
            actual_causes = {tuple(row[:4]): int(row[4]) for row in details}
            assert len(actual_causes) == len(details) and actual_causes == causes, text
            assert all(n > 0 for n in actual_causes.values()), text
            for pair, (_, unused, _) in actual.items():
                assert sum(n for key, n in actual_causes.items() if key[:2] == pair) == unused, text
        return True

    wait_until(check, timeout_s=90, interval_s=1, desc="accounting preemption rollup",
               tolerate=(AssertionError, RuntimeError))


def _scenario_report(c, used=0, unused=None):
    unused = unused or {}
    user = c.nodes[0].user
    # Resolve the default association from the actual report, including an empty account.
    wait_until(lambda: "|" in c.cli(["spur", "sreport", "preemptions", "--parsable", "--noheader"]),
               timeout_s=90, interval_s=1, desc="first accounting rollup")
    text = c.cli(["spur", "sreport", "preemptions", "--parsable", "--noheader"])
    pairs = [tuple(row.split("|")[:2]) for row in text.splitlines() if "|" in row]
    assert len(pairs) == 1 and pairs[0][0] == user, text
    pair = pairs[0]
    expected = {pair: (used, sum(unused.values()), 0)}
    causes = {(*pair, reason, initiator): count for (reason, initiator), count in unused.items()}
    _rollup_report(c, expected)
    _rollup_report(c, expected, causes=causes)
    c.restart_controller()
    _rollup_report(c, expected, causes=causes)


@pytest.mark.parametrize("outcome", ["used", "launch_failed"])
def test_preemption_rollups_survive_outage_restart_and_history_cleanup(rollup_cluster, outcome):
    c = rollup_cluster
    user = c.nodes[0].user
    used = int(outcome == "used")
    first_unused = {} if used else {("launch_failed", "system"): 1}
    first_totals = (used, sum(first_unused.values()), 0)
    first_causes = {(user, "rollup-a", reason, initiator): n
                    for (reason, initiator), n in first_unused.items()}
    for account in ("rollup-a", "rollup-b"):
        c.cli(["spur", "sacctmgr", "add", "account", f"name={account}"])
        c.cli(["spur", "sacctmgr", "add", "user", f"name={user}", f"account={account}"])
    victim = _submit(c, "rollup-victim", "victim-a", c.node_names[0])
    wait_job_state(c, victim, "R", timeout=60)
    _gate(c, victim, [0], True)
    hunter = _submit(c, "rollup-hunter", "hunter-a", c.node_names[0], extra=("--account=rollup-a",))
    _wait_epilog(c, victim, [0])
    _launch_gate(c, hunter, [0], True)
    _gate(c, victim, [0], False)
    _wait_prolog(c, hunter, [0])
    _usage(c, open_count=1)
    _rollup_report(c, {(user, "rollup-a"): (0, 0, 1)}, causes={})

    # Pause preserves the ephemeral published port that the controller is configured to use.
    c.nodes[0].exec(f"docker pause {shlex.quote(c._pg_container)}")
    try:
        if not used:
            c.nodes[0].exec(f"touch {shlex.quote(f'{c.remote_dir}/launch-{hunter}.fail')}")
        _launch_gate(c, hunter, [0], False)
        _usage(c, used=used, unused=first_unused)
        c.scancel(str(hunter))
        assert wait_job(c, hunter, timeout=60) == "CA"
        c.restart_controller()
        code, output = c.cli_with_exit(["spur", "sreport", "preemptions"])
        assert code != 0, output
        # Zero retention is floored to 120 seconds for accounting repair.
        time.sleep(130)
        assert f"PreemptionsUsed={used} PreemptionsUnused={sum(first_unused.values())}" in c.scontrol(
            "show", "job", str(hunter)
        )
    finally:
        c.nodes[0].exec(f"docker unpause {shlex.quote(c._pg_container)}")
    c._wait_pg(c.nodes[0])
    _rollup_report(c, {(user, "rollup-a"): first_totals}, causes=first_causes)
    wait_until(lambda: f"JobId={hunter} " not in c.cli_allow_fail(["scontrol", "show", "job", str(hunter)]),
               timeout_s=90, interval_s=1, desc="exported job leaving controller history")
    c.psql(f"DELETE FROM jobs WHERE job_id={int(hunter)}")
    c.restart_controller()
    _rollup_report(c, {(user, "rollup-a"): first_totals}, causes=first_causes)

    victim = _submit(c, "rollup-victim-b", "victim-a", c.node_names[1])
    wait_job_state(c, victim, "R", timeout=60)
    _gate(c, victim, [1], True)
    hunter = _submit(c, "rollup-hunter-b", "hunter-a", c.node_names[1], extra=("--account=rollup-b",))
    _wait_epilog(c, victim, [1])
    c.scancel(str(hunter))
    _gate(c, victim, [1], False)
    _usage(c, used=used, unused={**first_unused, ("cancelled", "user"): 1})
    expected = {(user, "rollup-a"): first_totals, (user, "rollup-b"): (0, 1, 0)}
    _rollup_report(c, expected, "--user", user)
    _rollup_report(c, {(user, "rollup-b"): (0, 1, 0)}, "--account", "rollup-b")
    causes = {**first_causes,
              (user, "rollup-b", "cancelled", "user"): 1}
    _rollup_report(c, expected, "--user", user, causes=causes)
    _rollup_report(c, {(user, "rollup-b"): (0, 1, 0)}, "--account", "rollup-b",
                   causes={(user, "rollup-b", "cancelled", "user"): 1})
    human = c.cli(["spur", "sreport", "preemptions", "--by-cause"])
    human_rows = [line.split() for line in human.splitlines() if line.strip()]
    assert human_rows[0] == ["User", "Account", "Reason", "Initiator", "Unused"], human
    expected_rows = [[*key, str(count)] for key, count in sorted(causes.items())]
    assert human_rows[1:] == expected_rows, human
    noheader = c.cli(["spur", "sreport", "preemptions", "--by-cause", "--noheader"])
    assert [line.split() for line in noheader.splitlines() if line.strip()] == expected_rows, noheader
    parsable = c.cli(["spur", "sreport", "preemptions", "--by-cause", "--parsable"])
    assert parsable.splitlines() == ["User|Account|Reason|Initiator|Unused",
                                     *["|".join(row) for row in expected_rows]], parsable
    for flags in ([], ["--by-cause"]):
        code, first = c.cli_with_exit(["spur", "sreport", "preemptions", "--parsable",
                                       "--noheader", "--limit", "1", *flags])
        assert code == 0, first
        first_rows = [line for line in first.splitlines() if "|" in line]
        if flags:
            assert first_rows == ([] if used else [f"{user}|rollup-a|launch_failed|system|1"]), first
        else:
            assert first_rows == [f"{user}|rollup-a|{used}|{sum(first_unused.values())}|0"], first
        token = re.search(r"Next page token: (.+)", first)
        assert token, first
        code, second = c.cli_with_exit(["spur", "sreport", "preemptions", "--parsable", "--noheader",
                                        "--limit", "1", "--page-token", token[1], *flags])
        assert code == 0, second
        assert "Next page token" not in second, second
        assert "rollup-a" not in second, second
        second_rows = [line for line in second.splitlines() if "|" in line]
        expected_second = f"{user}|rollup-b|cancelled|user|1" if flags else f"{user}|rollup-b|0|1|0"
        assert second_rows == [expected_second], second
    code, output = c.cli_with_exit(["spur", "sreport", "preemptions", "--start", "now-7days"])
    assert code != 0 and "lifetime" in output, output
