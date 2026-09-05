# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `srun --container-image` on bare-metal job steps (spur#777).

Inside an allocation, `srun --container-image=IMG cmd` must run cmd *inside* the
image, not on the host. The test image is a minimal rootfs (no /var, /opt, ...),
so the presence of a host-only directory cleanly distinguishes the host from the
container filesystem.
"""

import pytest

from conftest import _deploy_cluster


@pytest.fixture
def container_cluster(ssh_nodes, remote_bin_dir, cluster_config_overrides, tmp_path):
    """A rootful single-node cluster with the minimal test image built and
    shipped. Containerized steps enter mount/PID/user namespaces and pivot_root,
    which need a root agent; an unprivileged agent cannot set them up."""
    c = _deploy_cluster(
        ssh_nodes,
        remote_bin_dir,
        agent_as_root=True,
        config_overrides=cluster_config_overrides,
    )
    try:
        c.container_preflight()
        agent_user = c.spurd_agent_user(0)
        assert agent_user == "root", f"containerized steps need a root agent, got {agent_user!r}"
        c.container_image = c.build_container_image(tmp_path)
        yield c
    finally:
        c.teardown()


class TestSrunContainerStep:
    def test_step_runs_inside_the_image(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        # /var exists on the host but not in the minimal test image.
        code, out = cluster.salloc_run(
            f"srun --container-image={img} "
            f'sh -c "if [ -d /var ]; then echo HOST-FS; else echo CONTAINER-FS; fi"\n'
        )
        assert code == 0, out
        assert "CONTAINER-FS" in out, out
        assert "HOST-FS" not in out, out

    def test_step_exit_code_from_container(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        code, out = cluster.salloc_run(
            f'srun --container-image={img} sh -c "exit 13" || echo "ctn-exit=$?"\n'
        )
        assert code == 0, out
        assert "ctn-exit=13" in out, out
