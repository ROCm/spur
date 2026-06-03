# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""GPU E2E tests for the Spur scheduler.

These tests require GPU hardware on the nodes (ROCm/CUDA).
Tests are skipped automatically if no GPUs are detected (via gpu_preflight).

Set SPUR_TEST_GPU_VENV to a pre-existing venv path on the nodes with PyTorch
installed. If unset, tests assume a venv at {remote_dir}/venv and attempt to
provision it (requires python3-venv + network access on nodes).
"""

import os

from cluster import SpurCluster, parse_job_id, wait_job
from paths import native_host_deploy_dir


def _resolve_gpu_venv(cluster: SpurCluster) -> str:
    """
    Return the path to a Python venv with PyTorch on the nodes.
    Uses SPUR_TEST_GPU_VENV if set, otherwise provisions one.
    """
    env_venv = os.environ.get("SPUR_TEST_GPU_VENV", "")
    if env_venv:
        return env_venv

    venv_path = f"{cluster.remote_dir}/venv"
    torch_index = os.environ.get(
        "SPUR_TEST_TORCH_INDEX",
        "https://download.pytorch.org/whl/rocm6.3",
    )

    for node in cluster.nodes:
        node.exec(f"python3 -m venv '{venv_path}'")
        node.exec(
            f"'{venv_path}/bin/pip' install --quiet torch --index-url '{torch_index}'"
        )

    return venv_path


class TestGpuSingleNode:
    def test_hip_gpu_test(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)

        # Compile HIP gpu_test on the node if source and hipcc are available
        deploy = native_host_deploy_dir()
        hip_src = deploy / "gpu_test.hip"
        if hip_src.is_file():
            cluster.ship_file_to_all(hip_src, "gpu_test.hip")
            for node in cluster.nodes:
                node.exec_allow_fail(
                    f"command -v hipcc >/dev/null && "
                    f"hipcc -o '{cluster.remote_dir}/gpu_test' "
                    f"'{cluster.remote_dir}/gpu_test.hip' 2>/dev/null || true"
                )

        gpu_bin = f"{cluster.remote_dir}/gpu_test"
        script = cluster.write_file("gpu-1n.sh", f"#!/bin/bash\n'{gpu_bin}'\n")
        out_path = f"{cluster.remote_dir}/hip-1n.out"

        sb = cluster.sbatch(["-J", "hip-1n", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        diag = cluster.debug_job(job_id)
        assert "ALL PASS" in content, (
            f"HIP gpu_test must report ALL PASS.\n{diag}\noutput:\n{content}"
        )
        assert "GPU count:" in content, f"must report GPU count.\noutput:\n{content}"


class TestGpuMultiNode:
    def test_hip_gpu_test_two_node(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(2)

        deploy = native_host_deploy_dir()
        hip_src = deploy / "gpu_test.hip"
        if hip_src.is_file():
            cluster.ship_file_to_all(hip_src, "gpu_test.hip")
            for node in cluster.nodes:
                node.exec_allow_fail(
                    f"command -v hipcc >/dev/null && "
                    f"hipcc -o '{cluster.remote_dir}/gpu_test' "
                    f"'{cluster.remote_dir}/gpu_test.hip' 2>/dev/null || true"
                )

        gpu_bin = f"{cluster.remote_dir}/gpu_test"
        script = cluster.write_file("gpu-2n.sh", f"#!/bin/bash\n'{gpu_bin}'\n")
        out_path = f"{cluster.remote_dir}/hip-2n.out"

        sb = cluster.sbatch(["-J", "hip-2n", "-N", "2", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        diag = cluster.debug_job(job_id)
        assert "ALL PASS" in content, (
            f"2-node HIP gpu_test must report ALL PASS.\n{diag}\noutput:\n{content}"
        )

    def test_pytorch_distributed(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(2)

        venv_path = _resolve_gpu_venv(cluster)
        rd = cluster.remote_dir
        deploy = native_host_deploy_dir()

        # Ship the test script
        src = deploy / "distributed_test.py"
        if src.is_file():
            cluster.ship_file_to_all(src, "distributed_test.py")

        # Generate a wrapper that activates the venv and runs the script
        job_sh = cluster.write_file(
            "distributed_job.sh",
            f"#!/bin/bash\nsource '{venv_path}/bin/activate'\n"
            f"exec python3 '{rd}/distributed_test.py'\n",
        )
        out_path = f"{rd}/pt-dist.out"

        sb = cluster.sbatch(["-J", "pt-dist", "-N", "2", "-o", out_path, job_sh])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=600)
        content = cluster.read_output_on_any_node(out_path)
        diag = cluster.debug_job(job_id)
        assert "DONE" in content, (
            f"PyTorch distributed must report DONE.\n{diag}\noutput:\n{content}"
        )
        assert "TFLOPS" in content or "GPUs:" in content, (
            f"PyTorch output must contain TFLOPS or GPU count.\noutput:\n{content}"
        )

    def test_inference_two_node(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(2)

        venv_path = _resolve_gpu_venv(cluster)
        rd = cluster.remote_dir
        deploy = native_host_deploy_dir()

        # Ship the test script
        src = deploy / "inference_test.py"
        if src.is_file():
            cluster.ship_file_to_all(src, "inference_test.py")

        # Generate a wrapper that activates the venv and runs the script
        job_sh = cluster.write_file(
            "inference_job.sh",
            f"#!/bin/bash\nsource '{venv_path}/bin/activate'\n"
            f"exec python3 '{rd}/inference_test.py'\n",
        )
        out_path = f"{rd}/infer.out"

        sb = cluster.sbatch(["-J", "infer-2n", "-N", "2", "-o", out_path, job_sh])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=600)
        all_output = cluster.read_output_all_nodes(out_path)
        diag = cluster.debug_job(job_id)
        assert "INFERENCE_OK" in all_output, (
            f"Inference must report INFERENCE_OK.\n{diag}\noutput:\n{all_output}"
        )
        assert "Throughput:" in all_output, (
            f"Inference must report throughput.\noutput:\n{all_output}"
        )
        assert "nan" not in all_output.lower(), (
            f"Inference output contains NaN.\noutput:\n{all_output}"
        )
        assert "non-finite" not in all_output.lower(), (
            f"Inference output contains non-finite values.\noutput:\n{all_output}"
        )
