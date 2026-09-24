# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: GPU sharing between Spur and the managed k0s cluster, on nodes with no GPU.

Needs 3 nodes: node 0 is the k0s control plane, node 1 a shared worker, node 2
a worker that is not shared. Each node gets one GPU from a `[[devices.gres]]`
file entry on a render node that has no DRM card behind it, so the GPU is
unshareable and no Spur GPU job may land on the shared node. The real GPU path
runs in test_gpu_sharing_gpu.py on hardware.
"""

from __future__ import annotations

import json
import re
import time

import pytest

from cluster import SpurCluster, make_remote_dir, parse_job_id, job_state, wait_job

pytestmark = pytest.mark.k0s

LABEL = "spur.amd.com/gpu-sharing"
FAKE_RENDER = "/dev/dri/renderD200"
LINKS = {
    "/var/lib/kubelet/plugins_registry": "/var/lib/k0s/kubelet/plugins_registry",
    "/var/lib/kubelet/plugins": "/var/lib/k0s/kubelet/plugins",
}


@pytest.fixture
def sharing_cluster(ssh_nodes, remote_bin_dir):
    if len(ssh_nodes) < 3:
        pytest.skip(f"GPU sharing e2e needs 3 nodes (got {len(ssh_nodes)})")
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    c.provision()
    c.root_agent_preflight()
    sudo = c._sudo_prefix()
    for node in c.nodes:
        node.exec(f"{sudo}sh -c 'mkdir -p /dev/dri && "
                  f"(test -e {FAKE_RENDER} || mknod {FAKE_RENDER} c 226 200)'")
    try:
        c.start(
            config_overrides={
                "cluster": {"enabled": True, "cni": "kuberouter"},
                "devices": {
                    "auto_detect": False,
                    "gres": [{"name": "gpu", "file": FAKE_RENDER}],
                },
            },
            agent_as_root=True,
        )
    except Exception:
        c.teardown()
        raise
    yield c
    try:
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)
    except Exception:
        pass
    c.teardown()
    for node in c.nodes:
        node.exec_allow_fail(f"{sudo}rm -f {FAKE_RENDER}")


def _kubectl(c: SpurCluster, args: str) -> str:
    return c.nodes[0].exec_allow_fail(
        f"{c._sudo_prefix()}$(command -v k0s || echo /usr/local/bin/k0s) kubectl {args} 2>&1"
    )


def _kubectl_apply(c: SpurCluster, name: str, manifest: dict) -> None:
    path = f"{c.remote_dir}/{name}.json"
    c.nodes[0].write_file(path, json.dumps(manifest))
    out = _kubectl(c, f"apply -f {path}")
    assert re.search(r"created|configured|unchanged", out), out


def _wait(what: str, check, timeout: int = 300, every: int = 5):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = check()
        if last:
            return last
        time.sleep(every)
    raise TimeoutError(f"{what} not reached within {timeout}s (last: {last!r})")


def _label(c: SpurCluster, node: str) -> str:
    return _kubectl(c, f"get node {node.lower()} -o jsonpath='{{.metadata.labels.spur\\.amd\\.com/gpu-sharing}}'").strip()


def _link_target(c: SpurCluster, index: int, path: str) -> str:
    return c.nodes[index].exec_allow_fail(f"readlink {path} || true").strip()


def _show_node(c: SpurCluster, node: str) -> str:
    return c.scontrol("show", "node", node)


def _fake_slice(node: str) -> dict:
    """Shaped like the slice the AMD DRA driver publishes (captured on an MI300X)."""
    return {
        "apiVersion": "resource.k8s.io/v1",
        "kind": "ResourceSlice",
        "metadata": {"name": f"{node}-gpu.amd.com-e2e"},
        "spec": {
            "driver": "gpu.amd.com",
            "nodeName": node,
            "pool": {"name": node, "generation": 1, "resourceSliceCount": 1},
            "devices": [{
                "name": "gpu-9-136",
                "attributes": {
                    "resource.kubernetes.io/pciBusID": {"string": "0000:2f:00.0"},
                    "type": {"string": "amdgpu"},
                },
            }],
        },
    }


def _orphan(node: str, job_id: int) -> list[dict]:
    labels = {
        "app.kubernetes.io/managed-by": "spurd",
        "spur.amd.com/job-id": str(job_id),
        "spur.amd.com/run-attempt": "0",
        "spur.amd.com/node": node,
    }
    name = f"spur-job-{job_id}-0-{node}"
    claim = {
        "apiVersion": "resource.k8s.io/v1",
        "kind": "ResourceClaim",
        "metadata": {"name": name, "namespace": "spur-system", "labels": labels},
        "spec": {"devices": {"requests": [{
            "name": "gpu0",
            "exactly": {"deviceClassName": "gpu.amd.com", "count": 1},
        }]}},
    }
    pod = {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": "spur-system", "labels": labels},
        "spec": {
            "nodeSelector": {"kubernetes.io/hostname": node},
            "containers": [{"name": "pause", "image": "quay.io/k0sproject/pause:3.10.2-0"}],
        },
    }
    return [claim, pod]


class TestGpuSharingWithoutGpus:
    def test_shared_node_lifecycle(self, sharing_cluster):
        c = sharing_cluster
        cp, shared, plain = c.node_names[0], c.node_names[1], c.node_names[2]
        k_shared, k_plain = shared.lower(), plain.lower()

        c.k8s_up(["--control-plane-node", cp, "--gpu-sharing-nodes", shared])
        c.wait_k8s_phase("ready", timeout=600)

        # Label: `true` on the shared worker, `false` on the other one. Each spurd
        # writes its own label, and a worker has no admin kubeconfig, so this also
        # proves the scoped credential that the controller gets for a worker.
        _wait("shared label", lambda: _label(c, shared) == "true")
        _wait("plain label", lambda: _label(c, plain) == "false")
        sas = _kubectl(c, "-n spur-system get serviceaccounts -o name")
        assert f"spurd-gpu-sharing-{k_shared}" in sas, sas
        assert f"spurd-gpu-sharing-{k_plain}" in sas, sas

        # Kubelet plugin links only on the shared node.
        for path, target in LINKS.items():
            assert _link_target(c, 1, path) == target
            assert _link_target(c, 2, path) == ""

        # No ResourceSlice from gpu.amd.com: the node is unshareable, so a GPU
        # job waits while a CPU-only job still runs there.
        _wait("unshareable reason",
              lambda: "GpuUnshareable=no ResourceSlice" in _show_node(c, shared))
        out = _show_node(c, shared)
        assert "GpuSharing=yes" in out, out
        assert "GpuSharing=no" in _show_node(c, plain)

        script = c.write_file("gs.sh", "#!/bin/bash\necho ok\n")
        cpu = parse_job_id(c.sbatch(["-J", "gs-cpu", "-w", shared, "-o",
                                     f"{c.remote_dir}/gs-cpu.out", script]))
        gpu = parse_job_id(c.sbatch(["-J", "gs-gpu", "-w", shared, "--gres", "gpu:1",
                                     "-o", f"{c.remote_dir}/gs-gpu.out", script]))
        wait_job(c, cpu, timeout=120)
        time.sleep(10)
        assert job_state(c.squeue_all(), gpu) == "PD", c.squeue_all()
        c.scancel(str(gpu))

        # With a slice the node-level reason goes; the GPU stays unshareable
        # because no DRM card backs the render node.
        _kubectl_apply(c, "slice", _fake_slice(k_shared))
        _wait("slice seen",
              lambda: "GpuUnshareable" not in _show_node(c, shared)
              and "State=unshareable" in _show_node(c, shared))

        # A placeholder of a job that is not live on the node is deleted.
        for i, obj in enumerate(_orphan(k_shared, 999999)):
            _kubectl_apply(c, f"orphan-{i}", obj)
        _wait("orphans deleted",
              lambda: "spur-job-999999" not in _kubectl(
                  c, "-n spur-system get pods,resourceclaims -o name"),
              timeout=180)

        # Opt-out: label `false` at once, links go with no live placeholder.
        c.cli_as_user("root", ["spur", "node", "gpu-sharing", shared, "off"])
        _wait("opt-out label", lambda: _label(c, shared) == "false")
        _wait("links removed",
              lambda: all(_link_target(c, 1, p) == "" for p in LINKS), timeout=120)
        _wait("flag off", lambda: "GpuSharing=no" in _show_node(c, shared))
