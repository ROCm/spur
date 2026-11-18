# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Topology locality must preserve the placement that justified an eviction."""

import re

import pytest

from cluster import wait_job_state
from test_preemption_shared_allocations import _finish, _identity, _qos, _run, _submit, _victims


@pytest.fixture(params=["cancel", "requeue"])
def victim_mode(request):
    return request.param


@pytest.fixture
def cluster_config_overrides(victim_mode, ssh_nodes):
    if len(ssh_nodes) < 3:
        pytest.skip("requires three compute nodes")
    names = [node.exec("hostname -s").strip() for node in ssh_nodes]
    return {
        "nodes": [{"names": name, "cpus": 4, "memory_mb": 100} for name in names],
        "scheduler": {"preempt_type": "qos_priority", "preempt_exempt_time": 0},
        "auth": {"allow_root_jobs": True},
        "partitions": [{"name": "default", "nodes": ",".join(names[:3]), "default": True,
                        "state": "UP", "preempt_mode": victim_mode,
                        "default_time": "10:00", "max_time": "24:00:00"}],
        "topology": {"plugin": "tree", "switches": [
            {"name": "rack-a", "nodes": ",".join(names[:2])},
            {"name": "rack-b", "nodes": names[2]},
            {"name": "root", "switches": "rack-a,rack-b"},
        ]},
    }


@pytest.mark.parametrize("topology", ["tree", "block"])
def test_victim_release_starts_across_topology_groups(accounting_cluster, victim_mode, topology):
    c = accounting_cluster
    _qos(c, victim_mode)
    available, busy, shared = c.node_names[:3]
    full, full_path = _run(c, "protected-full", "shared-protected", busy, cpus=4)
    partial, partial_path = _run(c, "protected-partial", "shared-protected", shared)
    before = (_identity(c, full, full_path, 1), _identity(c, partial, partial_path, 2))
    victim, _ = _run(c, "eligible", "shared-low", shared, cpus=3)
    incoming, _ = _submit(c, "incoming", "shared-high", ",".join(c.node_names[:3]),
                          count=2, seconds=20, extra=(f"--topology={topology}",))
    wait_job_state(c, incoming, "R", timeout=60)
    show = c.scontrol("show", "job", str(incoming))
    allocated = re.search(r"\bNodeList=([^\s]+)", show).group(1)
    assert set(c.scontrol("show", "hostnames", allocated).split()) == {available, shared}, show
    _victims(c, [victim], incoming, victim_mode)
    assert (_identity(c, full, full_path, 1), _identity(c, partial, partial_path, 2)) == before
    _finish(c, incoming)
    assert (_identity(c, full, full_path, 1), _identity(c, partial, partial_path, 2)) == before
