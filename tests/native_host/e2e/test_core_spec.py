# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for core and memory specialization (SPUR-386).

Reserve a CPU for node services via ``cpu_spec_list`` on a rootful cluster, then
prove the controller computed the layout and spurd enforced it on the ``spur``
cgroup root: ``scontrol show node`` shows the spec fields, and the root's
``cpuset.cpus`` — which bounds every job, since all jobs run under it — excludes
the reserved CPU. spurd must be root to write the root, which the fixture checks.
"""

import re

import pytest

from cluster import parse_job_id, wait_job
from conftest import _deploy_cluster

# Reserve the core that owns CPU 1: every multi-core test node has it, and
# picking a fixed low ID keeps the assertions independent of the node's size.
SPEC_CPU = 1
RESERVED_MEMORY_MB = 2048

_ROOT_PROBE = """#!/bin/bash
echo "ROOT_CPUSET=$(cat /sys/fs/cgroup/spur/cpuset.cpus 2>/dev/null)"
echo "ROOT_MEMHIGH=$(cat /sys/fs/cgroup/spur/memory.high 2>/dev/null)"
echo SPEC_PROBE_OK
"""


def _expand(cpuset: str) -> set[int]:
    """Expand a cpuset list like ``0-2,5`` into a set of CPU IDs."""
    cpus: set[int] = set()
    for part in cpuset.split(","):
        part = part.strip()
        if not part:
            continue
        low, _, high = part.partition("-")
        cpus.update(range(int(low), int(high) + 1)) if high else cpus.add(int(low))
    return cpus


@pytest.fixture
def spec_cluster(ssh_nodes, remote_bin_dir):
    """Rootful cluster reserving one CPU and some memory on every node."""
    if len(ssh_nodes) < 1:
        pytest.skip("spec tests require at least one node in SPUR_TEST_NODES")
    fstype = ssh_nodes[0].exec_allow_fail("stat -fc %T /sys/fs/cgroup").strip()
    if "cgroup2fs" not in fstype:
        pytest.skip(f"node 0 is not running cgroup v2 (/sys/fs/cgroup is {fstype!r})")
    cores = int(ssh_nodes[0].exec("nproc").strip())
    if cores <= SPEC_CPU + 1:
        pytest.skip(f"need a node with more than {SPEC_CPU + 1} CPUs (node 0 has {cores})")

    c = _deploy_cluster(
        ssh_nodes,
        remote_bin_dir,
        agent_as_root=True,
        config_overrides={
            "nodes": [
                {
                    "names": "ALL",
                    "cpu_spec_list": str(SPEC_CPU),
                    "reserved_memory_mb": RESERVED_MEMORY_MB,
                }
            ],
        },
    )
    try:
        assert c.spurd_agent_user(0) == "root", "spec enforcement needs a rootful agent"
        yield c
    finally:
        c.teardown()


def _spec_list(cluster, node: str) -> set[int]:
    out = cluster.scontrol_show_node(node)
    m = re.search(r"CPUSpecList=(\S+)", out)
    assert m is not None, f"CPUSpecList not shown for a spec node:\n{out}"
    return _expand(m.group(1))


class TestCoreSpec:
    def test_scontrol_shows_spec_fields(self, spec_cluster):
        """The reserved CPU and memory surface on scontrol show node."""
        node = spec_cluster.node_names[0]
        out = spec_cluster.scontrol_show_node(node)
        assert SPEC_CPU in _expand(re.search(r"CPUSpecList=(\S+)", out).group(1)), (
            f"CPUSpecList should include the reserved CPU {SPEC_CPU}:\n{out}"
        )
        mem = re.search(r"MemSpecLimit=(\d+)", out)
        assert mem is not None and int(mem.group(1)) == RESERVED_MEMORY_MB, (
            f"expected MemSpecLimit={RESERVED_MEMORY_MB}:\n{out}"
        )

    def test_root_cpuset_excludes_the_spec_cpu(self, spec_cluster):
        """The spur cgroup root — and thus every job — excludes the reserved CPU."""
        node = spec_cluster.node_names[0]
        reserved = _spec_list(spec_cluster, node)

        script = spec_cluster.write_file("spec-probe.sh", _ROOT_PROBE)
        out_path = f"{spec_cluster.remote_dir}/spec-probe.out"
        sb = spec_cluster.sbatch(
            ["-J", "spec-probe", "-N", "1", "-w", node, "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(spec_cluster, job_id, timeout=120)
        content = spec_cluster.wait_output(out_path, "SPEC_PROBE_OK", timeout=120)

        m = re.search(r"ROOT_CPUSET=(\S*)", content)
        assert m and m.group(1), f"could not read the spur root cpuset:\n{content}"
        root = _expand(m.group(1))
        assert reserved.isdisjoint(root), (
            f"spur root cpuset {sorted(root)} must exclude reserved CPUs "
            f"{sorted(reserved)}:\n{content}"
        )
