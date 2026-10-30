# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Exercise resource handoffs through real eviction, epilog, and placement."""

import re
import shlex
import time

import pytest

from cluster import SpurCluster, make_remote_dir, parse_job_id, wait_job_state


@pytest.fixture
def handoff_cluster(ssh_nodes, remote_bin_dir):
    if len(ssh_nodes) < 3:
        pytest.skip("handoff tests require three nodes")
    c = SpurCluster(ssh_nodes[:3], make_remote_dir(), remote_bin_dir)
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
            'while [ -e "$gate" ]; do sleep 0.1; done\n',
            all_nodes=True,
        )
        c.start({
            "scheduler": {"preempt_type": "qos_priority", "complete_wait_secs": 300},
            "hooks": {"epilog": epilog},
            "auth": {"allow_root_jobs": True},
        }, kill_stale=False)
        for name, priority, targets, mode in [
            ("victim-a", 100, "", "cancel"),
            ("victim-b", 100, "", "cancel"),
            ("victim-r", 100, "", "requeue"),
            ("hunter-a", 10000, "victim-a,victim-r", "off"),
            ("hunter-b", 5000, "victim-b", "off"),
            ("bystander", 20000, "", "off"),
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
            node.exec_allow_fail(f"rm -f {shlex.quote(c.remote_dir)}/hold-*")
        c.teardown()


def _submit(c, name, qos, nodes=None, count=1):
    script = c.write_file(f"{name}.sh", "#!/bin/bash\nsleep 600\n", all_nodes=True)
    args = ["--exclusive", f"-N{count}", "-t", "10:00", "-J", name, "-q", qos]
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


@pytest.mark.parametrize("victim_qos", ["victim-a", "victim-r"])
def test_freed_node_goes_to_preemptor_over_higher_priority_bystander(handoff_cluster, victim_qos):
    c = handoff_cluster
    node = c.node_names[0]
    victim = _submit(c, "victim", victim_qos, node)
    wait_job_state(c, victim, "R", timeout=60)
    drains = victim_qos == "victim-a"
    if drains:
        _gate(c, victim, [0], True)
    bystander = _submit(c, "bystander", "bystander", node)
    hunter = _submit(c, "hunter", "hunter-a", node)
    if drains:
        _wait_epilog(c, victim, [0])
        _assert_pending_for_passes(c, [hunter, bystander])
        _gate(c, victim, [0], False)
    wait_job_state(c, hunter, "R", timeout=60)
    show = c.scontrol("show", "job", str(victim))
    assert f"PreemptedBy={hunter}" in show, show
    _state(c, bystander, "PENDING")
    show = _state(c, hunter, "RUNNING")
    assert f"NodeList={node}" in show, show
