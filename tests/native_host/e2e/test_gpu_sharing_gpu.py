# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E on GPU hardware: a Kubernetes pod and a Spur job share one node's GPUs.

Runs on one AMD GPU node that becomes a k0s `single` node. The AMD DRA driver is
installed from its release chart, so `helm` must be on the node. Run it by hand:

    SPUR_TEST_NODES=<gpu-node> pytest -m gpu test_gpu_sharing_gpu.py

The first test also checks the DRA name of every GPU, so run it once with the
GPUs in SPX and once in CPX to check the card lookup of partitions.
"""

from __future__ import annotations

import json
import re
import time

import pytest

from cluster import SpurCluster, make_remote_dir, parse_job_id, job_state, wait_job

pytestmark = [pytest.mark.k0s, pytest.mark.gpu]

DRA_CHART = ("https://github.com/ROCm/k8s-gpu-dra-driver/releases/download/"
             "v1.0.1/k8s-gpu-dra-driver-v1.0.1.tgz")
KUBECONFIG = "/tmp/spur-e2e-admin.conf"
POD = "gs-e2e-pod"


@pytest.fixture(scope="module")
def gpu_sharing_node(ssh_nodes, remote_bin_dir):
    node = ssh_nodes[0]
    if not node.exec_allow_fail("ls /dev/kfd 2>/dev/null").strip():
        pytest.skip("no /dev/kfd on the node")
    if not node.exec_allow_fail("command -v helm").strip():
        pytest.skip("helm is not installed on the node")
    c = SpurCluster([node], make_remote_dir(), remote_bin_dir)
    c.provision()
    c.root_agent_preflight()
    c.start(config_overrides={"cluster": {"enabled": True}}, agent_as_root=True)
    name = c.node_names[0]
    try:
        c.k8s_up(["--gpu-sharing-nodes", name])
        c.wait_k8s_phase("ready", timeout=600)
        _wait("admin kubeconfig", lambda: "server:" in node.exec_allow_fail(
            f"{c._sudo_prefix()}k0s kubeconfig admin > {KUBECONFIG} && cat {KUBECONFIG}"))
        _wait("shared label", lambda: _kubectl(c, f"get node {name.lower()} -o "
              "jsonpath='{.metadata.labels.spur\\.amd\\.com/gpu-sharing}'").strip() == "true")
        out = node.exec(
            f"{c._sudo_prefix()}helm install amd-gpu-dra {DRA_CHART} --kubeconfig {KUBECONFIG} "
            "--namespace kube-amd-gpu --create-namespace --set image.tag=v1.0.1 "
            "--set-string 'kubeletPlugin.nodeSelector.spur\\.amd\\.com/gpu-sharing=true' 2>&1"
        )
        assert "STATUS: deployed" in out, out
        _wait("ResourceSlice", lambda: _slice_devices(c), timeout=300)
        _wait("hold report", lambda: "GpuUnshareable" not in _show(c)
              and "State=free" in _show(c), timeout=180)
    except Exception:
        _teardown(c)
        raise
    yield c
    _teardown(c)


def _teardown(c: SpurCluster) -> None:
    sudo = c._sudo_prefix()
    _kubectl(c, f"delete pod {POD} --wait=false")
    _kubectl(c, f"delete resourceclaim {POD} --wait=false")
    c.nodes[0].exec_allow_fail(
        f"{sudo}helm uninstall amd-gpu-dra -n kube-amd-gpu --kubeconfig {KUBECONFIG}")
    try:
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=180)
    except Exception:
        pass
    c.teardown()
    c.nodes[0].exec_allow_fail(f"{sudo}rm -f {KUBECONFIG}")


def _kubectl(c: SpurCluster, args: str) -> str:
    return c.nodes[0].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl --kubeconfig {KUBECONFIG} {args} 2>&1")


def _wait(what: str, check, timeout: int = 300, every: int = 5):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = check()
        if last:
            return last
        time.sleep(every)
    raise TimeoutError(f"{what} not reached within {timeout}s (last: {last!r})")


def _show(c: SpurCluster) -> str:
    return c.scontrol("show", "node", c.node_names[0])


def _slice_devices(c: SpurCluster) -> dict[str, str]:
    """DRA device name -> pciBusID from the node's gpu.amd.com slices."""
    raw = _kubectl(c, "get resourceslices -o json")
    try:
        items = json.loads(raw)["items"]
    except (ValueError, KeyError):
        return {}
    node = c.node_names[0].lower()
    return {
        d["name"]: d.get("attributes", {})
        .get("resource.kubernetes.io/pciBusID", {}).get("string", "")
        for s in items
        if s["spec"].get("driver") == "gpu.amd.com" and s["spec"].get("nodeName") == node
        for d in s["spec"].get("devices", [])
    }


def _gpu_lines(c: SpurCluster) -> dict[str, str]:
    """DRA device name -> state from `scontrol show node`."""
    return dict(re.findall(r"Gpu=\S+ Device=(\S+) State=(.+)", _show(c)))


def _allocated(c: SpurCluster, namespace: str, which: str) -> list[str]:
    """Allocated devices of the claims that `which` (a name or `-l <selector>`) picks."""
    raw = _kubectl(c, f"-n {namespace} get resourceclaim {which} -o json")
    try:
        doc = json.loads(raw)
        claims = doc.get("items", [doc])
        return [r["device"] for claim in claims
                for r in claim["status"]["allocation"]["devices"]["results"]]
    except (ValueError, KeyError, TypeError):
        return []


def _apply(c: SpurCluster, objs: list[dict]) -> None:
    path = f"{c.remote_dir}/pod.json"
    c.nodes[0].write_file(path, json.dumps({"apiVersion": "v1", "kind": "List", "items": objs}))
    out = _kubectl(c, f"apply -f {path}")
    assert "created" in out or "configured" in out, out


class TestGpuSharingOnHardware:
    def test_every_gpu_has_the_dra_name_of_the_slice(self, gpu_sharing_node):
        c = gpu_sharing_node
        slice_devices = _slice_devices(c)
        spur_devices = _gpu_lines(c)
        assert spur_devices, _show(c)
        assert set(spur_devices) == set(slice_devices), (
            f"spur {sorted(spur_devices)} vs slice {sorted(slice_devices)}")
        assert all(s == "free" for s in spur_devices.values()), _show(c)

    def test_pod_and_job_get_different_gpus(self, gpu_sharing_node):
        c = gpu_sharing_node
        name = c.node_names[0].lower()
        total = len(_gpu_lines(c))
        claim = {
            "apiVersion": "resource.k8s.io/v1",
            "kind": "ResourceClaim",
            "metadata": {"name": POD, "namespace": "default"},
            "spec": {"devices": {"requests": [{
                "name": "gpu",
                "exactly": {"deviceClassName": "gpu.amd.com", "count": 1},
            }]}},
        }
        pod = {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": POD, "namespace": "default"},
            "spec": {
                "nodeSelector": {"kubernetes.io/hostname": name},
                "resourceClaims": [{"name": "gpu", "resourceClaimName": POD}],
                "containers": [{
                    "name": "pause",
                    "image": "quay.io/k0sproject/pause:3.10.2-0",
                    "resources": {"claims": [{"name": "gpu"}]},
                }],
            },
        }
        _apply(c, [claim, pod])
        pod_gpu = _wait("pod claim", lambda: _allocated(c, "default", POD), timeout=120)[0]
        _wait("hold seen", lambda: _gpu_lines(c).get(pod_gpu, "").startswith("held"))

        script = c.write_file("gs-gpu.sh", (
            "#!/bin/bash\n"
            "for d in /dev/dri/renderD*; do "
            "python3 -c \"import os,sys; os.close(os.open(sys.argv[1], os.O_RDWR))\" $d "
            "2>/dev/null && echo OPEN $d; done\n"
            "sleep 30\n"
        ))
        full = parse_job_id(c.sbatch(["-J", "gs-all", "--gres", f"gpu:{total}", "-o",
                                      f"{c.remote_dir}/gs-all.out", script]))
        one = parse_job_id(c.sbatch(["-J", "gs-one", "--gres", "gpu:1", "-o",
                                     f"{c.remote_dir}/gs-one.out", script]))
        placeholder = f"-l spur.amd.com/job-id={one}"
        job_gpu = _wait("placeholder claim",
                        lambda: _allocated(c, "spur-system", placeholder), timeout=180)
        assert job_gpu == [_wait("job hold",
                                 lambda: next((d for d, s in _gpu_lines(c).items()
                                               if s == f"job {one}"), None))]
        assert pod_gpu not in job_gpu
        assert job_state(c.squeue_all(), full) == "PD", c.squeue_all()

        wait_job(c, one, timeout=180)
        opened = re.findall(r"OPEN /dev/dri/renderD(\d+)",
                            c.nodes[0].read_file(f"{c.remote_dir}/gs-one.out"))
        assert opened == [job_gpu[0].rsplit("-", 1)[1]], opened
        _wait("placeholder deleted", lambda: f"spur-job-{one}-" not in _kubectl(
            c, "-n spur-system get pods,resourceclaims -o name"), timeout=120)
        c.scancel(str(full))
