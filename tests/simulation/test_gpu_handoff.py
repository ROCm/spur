# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""GPU-property and fabric-bound jobs honor preemption handoffs; no accelerator execution.

Needs preempt_type=qos_priority, accounting for QOS, an epilog that parks while the node's gate is held,
and an adapter op `gate <node> hold|release` for it. Nodes sim-node-[0-1] and [2-3] are the two vPods.
"""

import re

import pytest

NODES = [f"sim-node-{i}" for i in range(4)]
QOS = [("victim", 100, "", "cancel"), ("hunter", 10000, "victim", "off"), ("plain", 100, "", "off")]
EXPRESSIONS = [
    pytest.param("gpu.vram>=192GiB", id="gpu-property"),
    pytest.param("same(gpu.vpod_id)", id="fabric-bound"),
]


@pytest.fixture
def handoff(sim):
    for name, priority, preempt, mode in QOS:
        args = ["sacctmgr", "add", "qos", f"name={name}", f"priority={priority}", f"preemptmode={mode}"]
        if preempt:
            args.append(f"preempt={preempt}")
        sim.call("admin", *args, check=False)
    for name, *_ in QOS:
        sim.wait(lambda: _probe(sim, name))
    yield sim
    for node in NODES:
        sim.call("gate", node, "release")


def _probe(sim, qos):
    """The QOS caches refresh on a fixed interval, not on write; a held job proves the QOS is visible."""
    result = sim.cli("submit", "--parsable", "--hold", "--chdir", "/state", "-q", qos, "--wrap", "true", check=False)
    if result.returncode:
        return False
    sim.cli("cancel", result.stdout.strip())
    return True


def _submit(sim, *args, limit="10:00"):
    out = sim.cli("submit", "--parsable", "--chdir", "/state", "-t", limit, *args, "--wrap", "sleep 600")
    job = int(out.stdout.strip())
    sim.jobs.append(job)
    return job


def _field(sim, job, name):
    match = re.search(rf"\b{name}=(\S+)", sim.detail(job))
    return match.group(1) if match else None


def _gpu_job(sim, expression, node):
    return _submit(sim, "-N1", "-n1", "--gpus", "1", "--constraint", expression, "--nodelist", node,
                   "-q", "plain", limit="1:00")


def _drain_unused_node(sim):
    """A two-node owner waits for a filler pair while its victim holds sim-node-[0-1] in the epilog."""
    filler = _submit(sim, "-N2", "-n2", "--exclusive", "-q", "plain", "--nodelist", "sim-node-2,sim-node-3",
                     limit="3:00")
    victim = _submit(sim, "-N2", "-n2", "--exclusive", "-q", "victim", "--nodelist", "sim-node-0,sim-node-1")
    for job in (filler, victim):
        sim.running(job)
    sim.call("gate", "sim-node-0", "hold")
    sim.call("gate", "sim-node-1", "hold")
    owner = _submit(sim, "-N2", "-n2", "--exclusive", "-q", "hunter", limit="1:00")
    sim.wait(lambda: "JobState=COMPLETING" in sim.detail(victim))
    sim.wait(lambda: _field(sim, owner, "SchedNodeList") == "sim-node-[2-3]")
    return victim, owner


def _held_node_scene(sim):
    victim = _submit(sim, "-N1", "-n1", "--exclusive", "-q", "victim", "--nodelist", "sim-node-0")
    sim.running(victim)
    sim.call("gate", "sim-node-0", "hold")
    owner = _submit(sim, "-N1", "-n1", "--exclusive", "-q", "hunter", "--nodelist", "sim-node-0", limit="1:00")
    sim.wait(lambda: "JobState=COMPLETING" in sim.detail(victim))
    sim.wait(lambda: _field(sim, owner, "SchedNodeList") == "sim-node-0")
    return victim, owner


@pytest.mark.parametrize("expression", EXPRESSIONS)
def test_gpu_job_is_planned_on_a_node_its_owner_left_unused(handoff, expression):
    sim = handoff
    victim, owner = _drain_unused_node(sim)
    job = _gpu_job(sim, expression, "sim-node-0")
    sim.wait(lambda: _field(sim, job, "SchedNodeList") == "sim-node-0" and _field(sim, job, "StartTime"))
    assert "JobState=COMPLETING" in sim.detail(victim)
    assert "JobState=PENDING" in sim.detail(owner)
    assert "JobState=PENDING" in sim.detail(job)


@pytest.mark.parametrize("expression", EXPRESSIONS)
def test_gpu_job_is_planned_after_its_owner_on_a_held_node(handoff, expression):
    sim = handoff
    victim, owner = _held_node_scene(sim)
    job = _gpu_job(sim, expression, "sim-node-0")
    sim.wait(lambda: _field(sim, job, "SchedNodeList") == "sim-node-0" and _field(sim, job, "StartTime"))
    assert "JobState=COMPLETING" in sim.detail(victim)
    assert _field(sim, job, "StartTime") >= _field(sim, owner, "StartTime")


@pytest.mark.parametrize("expression", EXPRESSIONS)
def test_owner_starts_first_and_gpu_job_and_bystander_wait(handoff, expression):
    sim = handoff
    _, owner = _held_node_scene(sim)
    job = _gpu_job(sim, expression, "sim-node-0")
    bystander = _submit(sim, "-N1", "-n1", "--exclusive", "-q", "plain", "--nodelist", "sim-node-0", limit="1:00")
    sim.pending(job)
    sim.pending(bystander)
    sim.call("gate", "sim-node-0", "release")
    sim.running(owner)
    assert _field(sim, owner, "NodeList") == "sim-node-0"
    sim.pending(job)
    sim.pending(bystander)
