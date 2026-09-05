# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test: GPU device injection into a containerized job step.

A containerized `srun` step must receive the allocation's GPU device nodes
(injected into the container) the same way a batch container job does.

The node's GPU is registered via a config `[[devices.gres]]` entry mapping the
gres to the render node, so the test does not depend on the node having KFD
compute topology (auto-detect) — it exercises the injection path, which is what
this change added for steps. Needs a root agent (namespaces + device
injection).
"""

import pytest

from conftest import _deploy_cluster

pytestmark = pytest.mark.rootful

# The render node present on the test host; the gres maps to it so the scheduler
# has a GPU to allocate and the agent has a device node to inject.
RENDER_NODE = "/dev/dri/renderD128"

GRES_CONFIG = {
    "devices": {
        "auto_detect": False,
        "gres": [
            {
                "name": "gpu",
                "type": "test",
                "file": RENDER_NODE,
                "flags": ["amd_gpu_env"],
            }
        ],
    }
}


@pytest.fixture
def gpu_container_cluster(ssh_nodes, remote_bin_dir, tmp_path):
    if not ssh_nodes[0].exec_allow_fail(f"test -e {RENDER_NODE} && echo yes || true").strip():
        pytest.skip(f"{RENDER_NODE} not present on the test host")
    c = _deploy_cluster(ssh_nodes, remote_bin_dir, agent_as_root=True, config_overrides=GRES_CONFIG)
    try:
        c.container_preflight()
        c.container_image = c.build_container_image(tmp_path)
        yield c
    finally:
        c.teardown()


class TestSrunContainerGpu:
    def test_step_gets_gpu_device_injected(self, gpu_container_cluster):
        cluster = gpu_container_cluster
        img = cluster.container_image
        code, out = cluster.salloc_run(
            f'srun --container-image={img} '
            f'sh -c "if [ -e {RENDER_NODE} ]; then echo GPU-IN-CONTAINER; '
            f'else echo NO-GPU; fi"\n',
            salloc_args=["-N", "1", "--gres=gpu:1", "-t", "0:05"],
        )
        assert code == 0, out
        assert "GPU-IN-CONTAINER" in out, f"render node not injected into the container:\n{out}"
        assert "NO-GPU" not in out, out
