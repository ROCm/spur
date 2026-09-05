# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test: srun steps and interactive sessions are confined to the job cgroup.

Previously the step/interactive dispatch path spawned directly with no cgroup, so
those jobs ran in spurd's own service cgroup with no per-job limits (issue #802).
"""

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


class TestStepCgroup:
    def test_step_runs_in_job_cgroup(self, rootful_cluster):
        code, out = rootful_cluster.salloc_run(
            'srun bash -c "cat /proc/self/cgroup"\n'
        )
        assert code == 0, out
        # The step must be under /spur/job_<id>, not spurd's own service cgroup.
        assert "/spur/job_" in out, f"step not in a job cgroup:\n{out}"
        assert "spurd.service" not in out, f"step still in spurd's cgroup:\n{out}"
