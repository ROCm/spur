# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Resource-aware victim selection through submission, eviction, and execution."""

import re
import shlex

import pytest

from cluster import parse_job_id, wait_job, wait_job_state, wait_sacct_row, wait_until
from test_preemption_handoff import _assert_pending_for_passes


@pytest.fixture(params=["cancel", "requeue"])
def victim_mode(request):
    return request.param


@pytest.fixture
def cluster_config_overrides(victim_mode, ssh_nodes):
    names = [node.exec("hostname -s").strip() for node in ssh_nodes]
    return {
        "nodes": [{"names": name, "cpus": 4, "memory_mb": 100} for name in names],
        "scheduler": {"preempt_type": "qos_priority", "preempt_exempt_time": 0,
                      "preempt_victim_order": [],
                      "idle_fill_enabled": True, "idle_fill_exempt_secs": 0},
        "auth": {"allow_root_jobs": True},
        "partitions": [{"name": "default", "nodes": ",".join(names), "default": True,
                        "state": "UP", "preempt_mode": victim_mode,
                        "default_time": "10:00", "max_time": "24:00:00"}],
    }


def _qos(c, mode, reclaim=False):
    for name, priority, targets in [
        ("shared-low", 100, ""), ("shared-protected", 20000, ""),
        ("shared-high", 10000, "shared-low"),
    ]:
        args = ["add", "qos", f"name={name}", f"priority={priority}",
                f"preemptmode={mode if name == 'shared-low' else 'off'}"]
        if targets and not reclaim:
            args.append(f"preempt={targets}")
        if reclaim and name == "shared-low":
            args.append("idlefillpreemptable=yes")
        c.sacctmgr(args)


def _submit(c, label, qos, node, cpus=1, memory=0, extra=(), seconds=600, count=1):
    identity = f"{c.remote_dir}/{label}.identity"
    script = c.write_file(
        f"{label}.sh",
        '#!/bin/bash\n'
        f'echo "$$ $(awk \'{{print $22}}\' /proc/$$/stat)" > {shlex.quote(identity)}\n'
        f'exec sleep {seconds}\n',
        all_nodes=True,
    )
    args = [f"-N{count}", f"-n{count}", f"-c{cpus}", f"--mem={memory}",
            f"--nodelist={node}", "-J", label, "-q", qos, "-t", "10:00", *extra]
    job = parse_job_id(c.sbatch_when_qos_ready(args + [script], timeout=30))
    assert job is not None
    return job, identity


def _run(c, label, qos, node, **kwargs):
    job, path = _submit(c, label, qos, node, **kwargs)
    wait_job_state(c, job, "R", timeout=60)
    index = c.node_names.index(node.split(",")[0])
    wait_until(lambda: c.nodes[index].read_file(path).strip(), 30, interval_s=1,
               desc=f"{label} payload started")
    return job, path


def _identity(c, job, path, node_index=0):
    show = c.scontrol("show", "job", str(job))
    assert "JobState=RUNNING" in show, show
    node = c.nodes[node_index]
    recorded = node.read_file(path).strip()
    _, started = recorded.split()
    live = node.exec(
        f"awk '$2 == \"(sleep)\" && $22 == {int(started)} {{print $1, $22}}' "
        "/proc/[0-9]*/stat 2>/dev/null"
    ).strip()
    assert len(live.splitlines()) == 1 and live.split()[1] == started, (recorded, live)
    fields = tuple(re.search(rf"\b{key}=([^\s]+)", show).group(1)
                   for key in ["CPUs/Task", "NumNodes", "Restarts", "ReqTRES"])
    return recorded, live, fields


def _victims(c, jobs, incoming, mode):
    expected = "PreemptMode=Cancel" if mode == "cancel" else "PreemptMode=Requeue"
    for job in jobs:
        wait_until(lambda: f"PreemptedBy={incoming}" in c.scontrol("show", "job", str(job)),
                   60, interval_s=1, desc=f"selected victim {job}")
        show = c.scontrol("show", "job", str(job))
        assert expected in show, show
        if mode == "cancel":
            wait_job(c, job, timeout=60)
            row = wait_sacct_row(c, job, "JobID,State,PreemptedBy", timeout=60)
            assert "PREEMPTED" in row and str(incoming) in row, row


def _finish(c, incoming):
    wait_job(c, incoming, timeout=60)
    row = wait_sacct_row(c, incoming, "JobID,State,ExitCode", timeout=60)
    assert "COMPLETED" in row and "0:0" in row, row


def test_protected_job_keeps_its_allocation(accounting_cluster, victim_mode):
    c = accounting_cluster
    _qos(c, victim_mode)
    node = c.node_names[0]
    protected, path = _run(c, "protected", "shared-protected", node, cpus=2, memory=60)
    before = _identity(c, protected, path)
    a, _ = _run(c, "eligible-a", "shared-low", node, memory=20)
    b, spared_path = _run(c, "eligible-b", "shared-low", node, memory=20)
    spared = _identity(c, b, spared_path)
    incoming, _ = _submit(c, "incoming", "shared-high", node, memory=20, seconds=20)
    wait_job_state(c, incoming, "R", timeout=60)
    _victims(c, [a], incoming, victim_mode)
    assert _identity(c, protected, path) == before
    assert _identity(c, b, spared_path) == spared
    node_fields = c.scontrol("show", "node", node)
    assert re.search(r"\bCPUAlloc=4\b", node_fields), node_fields
    _finish(c, incoming)
    assert _identity(c, protected, path) == before


@pytest.mark.parametrize("cpu_request,memory_request", [(1, 0), (2, 0), (1, 50)])
def test_request_selects_only_enough_victims(
        accounting_cluster, victim_mode, cpu_request, memory_request):
    c = accounting_cluster
    _qos(c, victim_mode)
    node = c.node_names[0]
    victims = [_run(c, f"eligible-{i}", "shared-low", node, memory=25) for i in range(4)]
    enough = max(cpu_request, memory_request // 25)
    spared = [(job, path, _identity(c, job, path)) for job, path in victims[enough:]]
    incoming, _ = _submit(c, "incoming", "shared-high", node, cpus=cpu_request,
                          memory=memory_request, seconds=20)
    wait_job_state(c, incoming, "R", timeout=60)
    _victims(c, [job for job, _ in victims[:enough]], incoming, victim_mode)
    for job, path, identity in spared:
        assert _identity(c, job, path) == identity
    _finish(c, incoming)


@pytest.mark.parametrize("extra", [("--exclusive",), ("--mem=61",), ("-c3",)])
def test_incomplete_release_causes_no_eviction(accounting_cluster, victim_mode, extra):
    c = accounting_cluster
    _qos(c, victim_mode)
    node = c.node_names[0]
    protected, path = _run(c, "protected", "shared-protected", node, cpus=2, memory=60)
    a, a_path = _run(c, "eligible-a", "shared-low", node, memory=20)
    b, b_path = _run(c, "eligible-b", "shared-low", node, memory=20)
    before = [(job, path, _identity(c, job, path))
              for job, path in [(protected, path), (a, a_path), (b, b_path)]]
    preempted = c.sdiag_jobs_preempted()
    incoming, _ = _submit(c, "unplaceable", "shared-high", node, extra=extra)
    _assert_pending_for_passes(c, [incoming], passes=4)
    assert c.sdiag_jobs_preempted() == preempted
    for job, path, identity in before:
        assert _identity(c, job, path) == identity
    c.cli(["scancel", str(incoming)])
    control, _ = _submit(c, "control", "shared-high", node, seconds=20)
    wait_job_state(c, control, "R", timeout=60)
    _victims(c, [a], control, victim_mode)
    _finish(c, control)


def test_multinode_request_keeps_protected_cotenants(accounting_cluster, victim_mode):
    c = accounting_cluster
    if len(c.node_names) < 2:
        pytest.skip("requires two compute nodes")
    _qos(c, victim_mode)
    spared = []
    chosen = []
    for index, node in enumerate(c.node_names[:2]):
        job, path = _run(c, f"protected-{index}", "shared-protected", node, cpus=2)
        spared.append((job, path, index, _identity(c, job, path, index)))
        a, _ = _run(c, f"eligible-{index}-a", "shared-low", node)
        b, b_path = _run(c, f"eligible-{index}-b", "shared-low", node)
        chosen.append(a)
        spared.append((b, b_path, index, _identity(c, b, b_path, index)))
    incoming, _ = _submit(c, "incoming", "shared-high", ",".join(c.node_names[:2]),
                          count=2, seconds=20)
    wait_job_state(c, incoming, "R", timeout=60)
    _victims(c, chosen, incoming, victim_mode)
    for job, path, index, identity in spared:
        assert _identity(c, job, path, index) == identity
    _finish(c, incoming)


def test_reclaim_only_releases_the_shortfall(accounting_cluster, victim_mode):
    c = accounting_cluster
    _qos(c, victim_mode, reclaim=True)
    c.scontrol("reconfigure")
    node = c.node_names[0]
    protected, path = _run(c, "protected", "shared-protected", node, cpus=2)
    a, _ = _run(c, "borrowed-a", "shared-low", node)
    b, spared_path = _run(c, "borrowed-b", "shared-low", node)
    before = _identity(c, protected, path), _identity(c, b, spared_path)
    incoming, _ = _submit(c, "reclaimer", "shared-high", node, seconds=20)
    wait_job_state(c, incoming, "R", timeout=60)
    _victims(c, [a], incoming, "requeue")
    assert (_identity(c, protected, path), _identity(c, b, spared_path)) == before
    _finish(c, incoming)
