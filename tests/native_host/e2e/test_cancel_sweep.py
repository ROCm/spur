# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test: cancelling an allocation sweeps its job cgroup (issue #799).

A process left running inside the allocation's cgroup (a step that backgrounds
work, or an adopted session) must be killed when the allocation ends, without
depending on a site-configured epilog.
"""

import time

import pytest

from conftest import _deploy_cluster

pytestmark = pytest.mark.rootful


@pytest.fixture
def rootful_cluster(ssh_nodes, remote_bin_dir, cluster_config_overrides):
    fstype = ssh_nodes[0].exec_allow_fail("stat -fc %T /sys/fs/cgroup").strip()
    if "cgroup2fs" not in fstype:
        pytest.skip("node 0 is not cgroup v2")
    c = _deploy_cluster(ssh_nodes, remote_bin_dir, agent_as_root=True,
                        config_overrides=cluster_config_overrides)
    try:
        yield c
    finally:
        c.teardown()


class TestCancelSweep:
    def test_cancel_kills_residual_cgroup_process(self, rootful_cluster):
        c = rootful_cluster
        # A step backgrounds a process that outlives it; it stays in the
        # allocation cgroup. When salloc exits the allocation is cancelled.
        c.nodes[0].exec_allow_fail("pkill -x sleep")
        c.salloc_run(
            'srun bash -c "setsid sleep 300 </dev/null >/dev/null 2>&1 &"\n'
        )
        deadline = time.time() + 15
        leftover = "x"
        while time.time() < deadline:
            leftover = c.nodes[0].exec_allow_fail("pgrep -x sleep || true").strip()
            if not leftover:
                break
            time.sleep(1)
        assert not leftover, f"cancel left a residual cgroup process:\n{leftover}"
