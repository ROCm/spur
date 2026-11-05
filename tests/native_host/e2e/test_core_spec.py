# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for core and memory specialization.

Reserve a CPU and memory for node services on a rootful cluster, then prove the
controller computed the layout and spurd enforced it on the ``spur`` cgroup root,
and that spurd itself undoes it when specialization is removed.
"""

import re
import time

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
echo "MEMTOTAL_KB=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)"
echo "JOB_CPUS=$(awk '/^Cpus_allowed_list:/ {print $2}' /proc/self/status)"
echo SPEC_PROBE_OK
"""

_RESET_ROOT = (
    "sudo sh -c 'if [ -d /sys/fs/cgroup/spur ]; then "
    "echo > /sys/fs/cgroup/spur/cpuset.cpus 2>/dev/null; "
    "echo max > /sys/fs/cgroup/spur/memory.high 2>/dev/null; "
    "echo max > /sys/fs/cgroup/spur/memory.max 2>/dev/null; fi'"
)

_SPEC_NODES = {
    "nodes": [
        {
            "names": "ALL",
            "cpu_spec_list": str(SPEC_CPU),
            "reserved_memory_mb": RESERVED_MEMORY_MB,
        }
    ],
}


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
        config_overrides=_SPEC_NODES,
    )
    try:
        assert c.spurd_agent_user(0) == "root", "spec enforcement needs a rootful agent"
        yield c
    finally:
        c.teardown()
        # Safety net for a failed test: the `spur` root outlives spurd, and a
        # leftover reservation would leak into other tests on this node.
        for node in c.nodes:
            node.exec_allow_fail(_RESET_ROOT)


def _spec_list(cluster, node: str) -> set[int]:
    out = cluster.scontrol_show_node(node)
    m = re.search(r"CPUSpecList=(\S+)", out)
    assert m is not None, f"CPUSpecList not shown for a spec node:\n{out}"
    return _expand(m.group(1))


def _field(out: str, name: str) -> str:
    m = re.search(rf"{name}=(\S+)", out)
    assert m, f"{name} not shown:\n{out}"
    return m.group(1)


class TestCoreSpec:
    def test_scontrol_shows_spec_fields(self, spec_cluster):
        """The reserved CPU and memory surface on scontrol show node."""
        node = spec_cluster.node_names[0]
        out = spec_cluster.scontrol_show_node(node)
        reserved = _expand(_field(out, "CPUSpecList"))
        assert SPEC_CPU in reserved, out
        assert int(_field(out, "MemSpecLimit")) == RESERVED_MEMORY_MB, out
        assert int(_field(out, "CoreSpecCount")) >= 1, out
        assert int(_field(out, "CPUEfctv")) == int(_field(out, "CPUTot")) - len(reserved), out

    def test_root_and_jobs_exclude_the_spec_cpu_and_memory(self, spec_cluster):
        """The spur root, and a job's own affinity, exclude the reservation."""
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

        root = _expand(_field(content, "ROOT_CPUSET"))
        assert reserved.isdisjoint(root), (
            f"spur root cpuset {sorted(root)} must exclude {sorted(reserved)}:\n{content}"
        )
        job_cpus = _expand(_field(content, "JOB_CPUS"))
        assert job_cpus and reserved.isdisjoint(job_cpus), (
            f"job affinity {sorted(job_cpus)} must exclude {sorted(reserved)}:\n{content}"
        )
        target_mb = int(_field(content, "MEMTOTAL_KB")) // 1024 - RESERVED_MEMORY_MB
        assert int(_field(content, "ROOT_MEMHIGH")) == target_mb * 1024 * 1024, content

    def test_reservation_becomes_hard_on_an_idle_node(self, spec_cluster):
        """With no job running, memory.max drops to the reserved target."""
        node = spec_cluster.nodes[0]
        total_kb = int(node.exec("awk '/^MemTotal:/ {print $2}' /proc/meminfo").strip())
        target = (total_kb // 1024 - RESERVED_MEMORY_MB) * 1024 * 1024
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            mem_max = node.exec_allow_fail("cat /sys/fs/cgroup/spur/memory.max").strip()
            if mem_max == str(target):
                return
            time.sleep(2)
        pytest.fail(f"memory.max={mem_max!r}, expected {target} on an idle node")

    def test_an_uncomputable_layout_keeps_the_root_across_restarts(self, spec_cluster):
        """A full restart with a layout the controller cannot compute drains the
        node and leaves the root's spec cpuset in place instead of resetting it."""
        node = spec_cluster.nodes[0]
        name = spec_cluster.node_names[0]
        reserved = _spec_list(spec_cluster, name)
        assert reserved.isdisjoint(_expand(node.exec("cat /sys/fs/cgroup/spur/cpuset.cpus")))

        impossible = {
            "nodes": [
                {
                    "names": "ALL",
                    "cpu_spec_list": str(SPEC_CPU),
                    "reserved_memory_mb": 1 << 40,
                }
            ],
        }
        # start() waits for every node to be idle, which a drained node never is.
        spec_cluster.stop()
        spec_cluster.start_controller(impossible)
        spec_cluster.start_agents()

        deadline = time.monotonic() + 90
        out = ""
        while time.monotonic() < deadline:
            out = spec_cluster.scontrol_show_node(name)
            if "core-layout:" in out:
                break
            time.sleep(2)
        assert "core-layout:" in out, out
        root = _expand(node.exec("cat /sys/fs/cgroup/spur/cpuset.cpus"))
        assert root and reserved.isdisjoint(root), (
            f"root cpuset {sorted(root)} must still exclude {sorted(reserved)}"
        )

    def test_spurd_resets_the_root_when_specialization_is_removed(self, spec_cluster):
        """Restarting without specialization makes spurd undo its own root limits."""
        node = spec_cluster.nodes[0]
        assert node.exec("cat /sys/fs/cgroup/spur/cpuset.cpus").strip(), "spec not applied"

        spec_cluster.stop()
        spec_cluster.start(config_overrides={}, agent_as_root=True)

        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            cpuset = node.exec_allow_fail("cat /sys/fs/cgroup/spur/cpuset.cpus").strip()
            mem_max = node.exec_allow_fail("cat /sys/fs/cgroup/spur/memory.max").strip()
            if not cpuset and mem_max == "max":
                return
            time.sleep(2)
        pytest.fail(f"spur root not reset: cpuset.cpus={cpuset!r} memory.max={mem_max!r}")
