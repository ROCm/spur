# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for zero-GPU visibility deny.

A job allocated no GPUs must have the vendor GPU selectors set to a "no device"
token (invalid index -1 for ROCm/CUDA, `void` for the nvidia-container-runtime)
and SPUR_JOB_GPUS empty, so runtimes see no devices instead of defaulting to
all-visible. These need no GPU hardware; the assertions are on env values.
"""

from cluster import parse_job_id, wait_job

# Deny tokens gpu_deny_visibility writes for a zero-GPU job: -1 for the ROCm/CUDA
# index selectors, `void` for nvidia-container-runtime, empty for Spur's own list.
_EXPECTED_DENY = {
    "ROCR_VISIBLE_DEVICES": "-1",
    "HIP_VISIBLE_DEVICES": "-1",
    "CUDA_VISIBLE_DEVICES": "-1",
    "GPU_DEVICE_ORDINAL": "-1",
    "ZE_AFFINITY_MASK": "-1",
    "NVIDIA_VISIBLE_DEVICES": "void",
    "SPUR_JOB_GPUS": "",
}


def _probe_body() -> str:
    # ${VAR+SET} prints "SET" only when VAR is defined, distinguishing a denied
    # value from an unset var (the pre-fix state).
    lines = "\n".join(f'echo "{v}:${{{v}+SET}}:[${{{v}-}}]"' for v in _EXPECTED_DENY)
    return f"{lines}\necho DENY_OK\n"


def _assert_all_denied(output: str, context: str) -> None:
    for var, expected in _EXPECTED_DENY.items():
        assert f"{var}:SET:[{expected}]" in output, (
            f"{var} must be denied ([{expected}]) for a zero-GPU job.\n{context}"
        )


class TestGpuVisibilityDeny:
    def test_zero_gpu_batch_job_denies_visibility(self, cluster):
        script = cluster.write_file("gpu-deny.sh", f"#!/bin/bash\n{_probe_body()}")
        out_path = f"{cluster.remote_dir}/gpu-deny.out"

        sb = cluster.sbatch(["-J", "gpu-deny", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job(cluster, job_id, timeout=120)
        content = cluster.wait_output(out_path, "DENY_OK", timeout=120)
        _assert_all_denied(content, f"{cluster.debug_job(job_id)}\noutput:\n{content}")

    def test_zero_gpu_srun_step_denies_visibility(self, cluster):
        # A standalone srun step launches via run_command, a different path than
        # sbatch, so a zero-GPU step must be denied there too.
        code, output = cluster.srun_with_exit(["-N", "1", "bash", "-c", _probe_body()])
        assert code == 0, f"srun step failed (exit {code}):\n{output}"
        _assert_all_denied(output, f"exit {code}\noutput:\n{output}")
