# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for configured node resource caps.

Configure `[[nodes]]` caps below whatever the real hardware reports, then assert
that the schedulable inventory (`scontrol show node`) reflects the cap and that an
over-cap job stays pending on Resources. Caps are kept tiny (4 CPUs / 4 GiB) so the
test holds on any real node, which always detects more.
"""

import re

import pytest

from cluster import job_state, parse_job_id, wait_job_state
from conftest import _deploy_cluster

# Caps set well below any real testbed node (the smallest is an 8-core / ~8 GiB
# VM) so the clamp is always observable: a node reporting more than these values
# proves the configured cap won over autodetection.
CAP_CPUS = 4
CAP_MEMORY_MB = 4096           # 4 GiB cap on schedulable memory
RESERVED_MEMORY_MB = 1024      # 1 GiB held back for OS/runtime


def _scontrol_field(out: str, key: str) -> int:
    """Parse an integer `key=<n>` from `scontrol show node` output."""
    m = re.search(rf"{key}=(\d+)", out)
    assert m is not None, f"{key} not found in scontrol output:\n{out}"
    return int(m.group(1))


@pytest.fixture
def resource_cap_cluster(ssh_nodes, remote_bin_dir):
    """Cluster whose [[nodes]] caps (via the ALL wildcard) sit below real hardware.

    deep_merge replaces the whole `nodes` list, so this single ALL-matching entry
    fully supersedes the harness default of cpus=64/memory_mb=262144 and applies
    the tiny caps to every node without needing hostnames resolved up front.
    """
    c = _deploy_cluster(
        ssh_nodes,
        remote_bin_dir,
        config_overrides={
            "nodes": [
                {
                    "names": "ALL",
                    "cpus": CAP_CPUS,
                    "memory_mb": CAP_MEMORY_MB,
                    "reserved_memory_mb": RESERVED_MEMORY_MB,
                }
            ],
        },
    )
    yield c
    c.teardown()


class TestNodeResourceCaps:
    """Configured caps bound the schedulable inventory on real hardware."""

    def test_cpu_cap_below_detected_is_enforced(self, resource_cap_cluster):
        """scontrol shows the configured CPU cap, not the larger detected count."""
        node = resource_cap_cluster.node_names[0]
        out = resource_cap_cluster.scontrol_show_node(node)
        cpu_tot = _scontrol_field(out, "CPUTot")
        assert cpu_tot == CAP_CPUS, (
            f"expected CPUTot clamped to {CAP_CPUS}, got {cpu_tot}. Real node has "
            f"more cores, so a value above {CAP_CPUS} means the cap was ignored:\n{out}"
        )

    def test_memory_cap_below_detected_is_enforced(self, resource_cap_cluster):
        """RealMemory reflects the configured cap, below the detected total.

        The cap (4 GiB) is the binding constraint on any real node, so this proves
        the memory cap wins over autodetection. The reserved_memory_mb subtraction
        and cap/reserved composition math are exhaustively covered by the
        `ResourceSet::clamped` unit tests; here we prove the config→scontrol wiring
        on real hardware.
        """
        node = resource_cap_cluster.node_names[0]
        out = resource_cap_cluster.scontrol_show_node(node)
        real_mem = _scontrol_field(out, "RealMemory")
        assert real_mem == CAP_MEMORY_MB, (
            f"expected RealMemory clamped to {CAP_MEMORY_MB}, got {real_mem}. Real "
            f"node has more memory, so a larger value means the cap was ignored:\n{out}"
        )

    def test_job_above_cpu_cap_never_runs(self, resource_cap_cluster):
        """A job asking for more CPUs than the cap stays pending, never runs."""
        # Request one more CPU than the node advertises after clamping.
        code, out = resource_cap_cluster.sbatch_with_exit(
            ["-c", str(CAP_CPUS + 1), "--wrap", "true"]
        )
        job_id = parse_job_id(out)
        assert job_id is not None, f"sbatch did not return a job id:\n{out}"

        # It must never reach RUNNING; it pends because no node can satisfy it.
        wait_job_state(resource_cap_cluster, job_id, "PD", timeout=30)
        sq = resource_cap_cluster.squeue_all()
        assert job_state(sq, job_id) == "PD", (
            f"job {job_id} requesting {CAP_CPUS + 1} CPUs should pend on a "
            f"{CAP_CPUS}-CPU-capped node, got:\n{sq}"
        )

    def test_job_within_cap_runs(self, resource_cap_cluster):
        """A job within the cap schedules and runs, proving the node still works."""
        # A blocking command keeps the job in RUNNING long enough for the poller to
        # observe it; `true` could finish between polls and never be seen as R.
        code, out = resource_cap_cluster.sbatch_with_exit(
            ["-c", str(CAP_CPUS), "--wrap", "sleep 30"]
        )
        job_id = parse_job_id(out)
        assert job_id is not None, f"sbatch did not return a job id:\n{out}"
        try:
            wait_job_state(resource_cap_cluster, job_id, "R", timeout=30)
        finally:
            resource_cap_cluster.cli_allow_fail(["scancel", str(job_id)])
