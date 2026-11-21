# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Shared GPU preemption on real devices; hardware-free beds explicitly skip."""

import re

import pytest

from cluster import wait_job_state
from test_preemption_shared_allocations import (
    _finish, _identity, _qos, _run, _submit, _victims,
)

pytestmark = pytest.mark.gpu


@pytest.fixture(params=["cancel", "requeue"])
def victim_mode(request):
    return request.param


@pytest.fixture
def gpu_preflight(ssh_nodes):
    if not all(node.exec_allow_fail("test -e /dev/kfd && echo present").strip() == "present"
               for node in ssh_nodes):
        pytest.skip("requires AMD GPU devices exposed to the test nodes")


@pytest.fixture
def cluster_config_overrides(victim_mode, ssh_nodes, gpu_preflight):
    names = [node.exec("hostname -s").strip() for node in ssh_nodes]
    return {
        "nodes": [{"names": name, "cpus": 4, "memory_mb": 8192} for name in names],
        "scheduler": {"preempt_type": "qos_priority", "preempt_exempt_time": 0},
        "auth": {"allow_root_jobs": True},
        "partitions": [{"name": "default", "nodes": ",".join(names), "default": True,
                        "state": "UP", "preempt_mode": victim_mode,
                        "default_time": "10:00", "max_time": "24:00:00"}],
    }


def _visible(c, job):
    output = c.cli(["srun", f"--jobid={job}", "--overlap", "bash", "-c",
                    'test -n "$ROCR_VISIBLE_DEVICES" && printf "%s\\n" "$ROCR_VISIBLE_DEVICES"'])
    devices = output.strip().split(",")
    assert devices and all(devices), output
    return tuple(devices)


@pytest.mark.parametrize("request_form", ["--gpus-per-node=1", "--gpus=1"])
def test_gpu_request_releases_one_device_and_spares_its_cotenant(
        accounting_cluster, victim_mode, request_form):
    c = accounting_cluster
    node = c.node_names[0]
    show = c.scontrol("show", "node", node)
    counts = [int(count) for count in re.findall(r"gpu(?::[^:,\s]+)?:(\d+)", show)]
    if sum(counts) < 3:
        pytest.skip("requires at least three schedulable GPUs on one node")
    _qos(c, victim_mode)
    protected, path = _run(c, "gpu-protected", "shared-protected", node,
                           extra=(f"--gpus-per-node={sum(counts) - 2}",), memory=1024)
    a, _ = _run(c, "gpu-a", "shared-low", node, extra=("--gpus-per-node=1",), memory=1024)
    b, b_path = _run(c, "gpu-b", "shared-low", node,
                     extra=("--gpus-per-node=1",), memory=1024)
    before = _identity(c, protected, path), _identity(c, b, b_path)
    visibility = _visible(c, protected), _visible(c, b)
    incoming, _ = _submit(c, "gpu-incoming", "shared-high", node,
                          extra=(request_form,), memory=1024, seconds=20)
    wait_job_state(c, incoming, "R", timeout=60)
    _victims(c, [a], incoming, victim_mode)
    assert (_identity(c, protected, path), _identity(c, b, b_path)) == before
    assert (_visible(c, protected), _visible(c, b)) == visibility
    assert len(_visible(c, incoming)) == 1
    _finish(c, incoming)
