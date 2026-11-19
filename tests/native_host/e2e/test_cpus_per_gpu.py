# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for ``--cpus-per-gpu`` / ``--mem-per-gpu``.

A job's CPUs and memory sized per GPU reach its cpuset, its cgroup memory
ceiling, its environment and ``scontrol show job``; a partition's
``def_cpu_per_gpu`` sizes jobs that give no ``-c``; conflicting or GPU-less
per-GPU options are refused by the CLI before anything is submitted.
"""

import re

import pytest

from cluster import (
    RESOURCE_PROBE,
    expand_cpulist,
    key_values,
    parse_job_id,
)

MIB = 1024 * 1024
CPU_CONFLICT = "--cpus-per-gpu cannot be combined with --cpus-per-task (-c)"
MEM_CONFLICT = "--mem, --mem-per-cpu, and --mem-per-gpu are mutually exclusive"
MEM_ENV_CONFLICT = "SBATCH_MEM, SBATCH_MEM_PER_CPU, and SBATCH_MEM_PER_GPU are mutually exclusive"


def _sbatch_env(cluster, args: list[str], env: dict[str, str] | None = None) -> tuple[int, str]:
    """sbatch with extra input variables; returns (exit code, combined output)."""
    return cluster.nodes[0].exec_with_exit(
        cluster._cli_command(["sbatch"] + args, extra_env=env)
    )


def _hold_probe(cluster, name: str, sbatch_args: list[str],
                env: dict[str, str] | None = None) -> tuple[int, dict[str, str], str]:
    """Submit a probe that stays running; return job ID, probe values and
    ``scontrol show job`` taken while it runs. The caller cancels the job."""
    script = cluster.write_file(f"{name}.sh", RESOURCE_PROBE + "sleep 300\n")
    out_path = f"{cluster.remote_dir}/{name}.out"
    code, sb = _sbatch_env(
        cluster,
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-o", out_path]
        + sbatch_args + [script],
        env,
    )
    job_id = parse_job_id(sb)
    assert code == 0 and job_id is not None, f"sbatch failed ({code}): {sb}"
    content = cluster.wait_output(out_path, "RESOURCE_PROBE_OK", timeout=120)
    detail = cluster.scontrol("show", "job", str(job_id))
    assert "RESOURCE_PROBE_OK" in content, (
        f"probe never ran\n{cluster.debug_job(job_id)}\n{content}"
    )
    return job_id, key_values(content), detail


def _req_cpus(detail: str) -> int:
    m = re.search(r"ReqTRES=cpu=(\d+)", detail)
    assert m, detail
    return int(m.group(1))


def _require_gpus(cluster, needed: int) -> None:
    cluster.gpu_preflight(1)
    have = cluster.node_gpu_count(cluster.node_names[0])
    if have < needed:
        pytest.skip(f"need >= {needed} GPUs on node 0 (got {have})")


@pytest.mark.rootful
class TestPerGpuSizing:
    def test_cpus_per_gpu_sizes_the_job(self, gpu_cluster):
        cluster = gpu_cluster
        _require_gpus(cluster, 2)
        job_id, values, detail = _hold_probe(
            cluster, "cpg", ["--gres=gpu:2", "--cpus-per-gpu=3"]
        )
        cluster.scancel(job_id)

        assert values["SPUR_CPUS_ON_NODE"] == "6", values
        assert len(expand_cpulist(values["CPUS"])) == 6, values
        assert values["SPUR_CPUS_PER_GPU"] == "3" and values["SLURM_CPUS_PER_GPU"] == "3", values
        assert "CpusPerTres=gpu:3" in detail, detail
        assert _req_cpus(detail) == 6, detail

    def test_mem_per_gpu_sizes_the_job_memory(self, gpu_cluster):
        cluster = gpu_cluster
        _require_gpus(cluster, 2)
        job_id, values, detail = _hold_probe(
            cluster, "mpg", ["--gres=gpu:2", "--mem-per-gpu=1G"]
        )
        cluster.scancel(job_id)

        assert values["JOB_MEM_MAX"] == str(2 * 1024 * MIB), values
        assert values["SPUR_MEM_PER_GPU"] == "1024" and values["SLURM_MEM_PER_GPU"] == "1024", values
        assert "MemPerTres=gpu:1024" in detail, detail

    def test_salloc_exports_the_per_gpu_options(self, gpu_cluster):
        cluster = gpu_cluster
        _require_gpus(cluster, 2)
        code, out = cluster.salloc_run(
            "for v in SPUR_CPUS_PER_GPU SLURM_CPUS_PER_GPU SPUR_MEM_PER_GPU SLURM_MEM_PER_GPU; do\n"
            '  echo "$v=${!v-unset}"\n'
            "done",
            salloc_args=["-N", "1", "-t", "0:05", "--gpus-per-node=2",
                         "--cpus-per-gpu=3", "--mem-per-gpu=2G"],
        )
        values = key_values(out)
        assert code == 0, out
        assert values.get("SPUR_CPUS_PER_GPU") == "3" and values.get("SLURM_CPUS_PER_GPU") == "3", out
        assert values.get("SPUR_MEM_PER_GPU") == "2048" and values.get("SLURM_MEM_PER_GPU") == "2048", out

    def test_explicit_option_drops_the_env_one(self, gpu_cluster):
        cluster = gpu_cluster
        _require_gpus(cluster, 1)
        cpu_job, _, cpu_detail = _hold_probe(
            cluster, "env-cpg", ["--gres=gpu:1", "-c", "2"], {"SBATCH_CPUS_PER_GPU": "8"}
        )
        cluster.scancel(cpu_job)
        mem_job, _, mem_detail = _hold_probe(
            cluster, "env-mpg", ["--gres=gpu:1", "--mem=1G"], {"SBATCH_MEM_PER_GPU": "4G"}
        )
        cluster.scancel(mem_job)

        assert _req_cpus(cpu_detail) == 2 and "CpusPerTres" not in cpu_detail, cpu_detail
        assert "MinMemoryNode=1024M" in mem_detail and "MemPerTres" not in mem_detail, mem_detail

    def test_parent_jobs_per_gpu_env_does_not_size_a_nested_submit(self, gpu_cluster):
        cluster = gpu_cluster
        _require_gpus(cluster, 1)
        inherited = {
            "SPUR_CPUS_PER_GPU": "8", "SLURM_CPUS_PER_GPU": "8",
            "SPUR_MEM_PER_GPU": "1024", "SLURM_MEM_PER_GPU": "1024",
        }
        job_id, _, detail = _hold_probe(cluster, "nested", ["--gres=gpu:1"], inherited)
        cluster.scancel(job_id)

        assert "CpusPerTres" not in detail and "MemPerTres" not in detail, detail
        assert _req_cpus(detail) == 1, detail


@pytest.mark.rootful
class TestPartitionDefaultCpusPerGpu:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "partitions": [
                {"name": "default", "state": "UP", "default": True, "nodes": "ALL",
                 "max_time": "24:00:00", "default_time": "10:00"},
                {"name": "gpudef", "state": "UP", "nodes": "ALL",
                 "max_time": "24:00:00", "default_time": "10:00", "def_cpu_per_gpu": 4},
            ],
        }

    def test_default_applies_only_without_c(self, gpu_cluster):
        cluster = gpu_cluster
        _require_gpus(cluster, 2)
        dflt_job, dflt, dflt_detail = _hold_probe(
            cluster, "defcpg", ["-p", "gpudef", "--gres=gpu:2"]
        )
        cluster.scancel(dflt_job)
        c_job, _, c_detail = _hold_probe(
            cluster, "defcpg-c", ["-p", "gpudef", "--gres=gpu:2", "-c", "3"]
        )
        cluster.scancel(c_job)

        assert dflt["SPUR_CPUS_ON_NODE"] == "8", dflt
        assert "CpusPerTres=gpu:4" in dflt_detail, dflt_detail
        assert _req_cpus(c_detail) == 3 and "CpusPerTres" not in c_detail, c_detail


class TestPerGpuConflicts:
    """Refused by the CLI, so no GPUs or root are needed."""

    @pytest.mark.parametrize(
        ("args", "env", "message"),
        [
            (["--gres=gpu:1", "-c", "8", "--cpus-per-gpu=16"], {}, CPU_CONFLICT),
            (["--gres=gpu:1", "--mem=1G", "--mem-per-gpu=1G"], {}, MEM_CONFLICT),
            (["--gres=gpu:1"], {"SBATCH_MEM_PER_GPU": "1G", "SBATCH_MEM_PER_CPU": "512M"},
             MEM_ENV_CONFLICT),
            (["--cpus-per-gpu=2"], {}, "--cpus-per-gpu requires a GPU request"),
            (["--mem-per-gpu=1G"], {}, "--mem-per-gpu requires a GPU request"),
        ],
        ids=["cpu-both-explicit", "mem-both-explicit", "mem-both-env",
             "cpus-per-gpu-without-gpus", "mem-per-gpu-without-gpus"],
    )
    def test_refused_before_submission(self, cluster, args, env, message):
        script = cluster.write_file("conflict.sh", "#!/bin/bash\ntrue\n")
        code, out = _sbatch_env(cluster, args + [script], env)
        assert code != 0 and message in out, f"exit {code}: {out}"
        assert "Submitted batch job" not in out, out

    def test_directives_conflict_like_options(self, cluster):
        script = cluster.write_file(
            "conflict-directives.sh",
            "#!/bin/bash\n#SBATCH --gres=gpu:1\n#SBATCH -c 2\n#SBATCH --cpus-per-gpu=4\ntrue\n",
        )
        code, out = _sbatch_env(cluster, [script])
        assert code != 0 and CPU_CONFLICT in out, f"exit {code}: {out}"
