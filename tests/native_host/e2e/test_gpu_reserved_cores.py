# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for GPU-reserved cores (``restricted_cores_per_gpu``).

Each GPU gets cores of its own on its NUMA node: GPU jobs take their GPUs'
reserved CPUs, CPU-only jobs only the rest. Expectations are derived from the
node's own ``scontrol show node`` layout, never from hardcoded CPU IDs.
"""

import re

import pytest

from cluster import (
    RESOURCE_PROBE,
    expand_cpulist,
    key_values,
    node_reserved_cpus,
    numa_cpus,
    parse_job_id,
    restricted_cores_config,
    threads_per_core,
    wait_job_state,
    wait_until,
)


def _hold_probe(cluster, name: str, sbatch_args: list[str]) -> tuple[int, dict[str, str]]:
    """Submit a probe that stays running; return its job ID and probe values."""
    script = cluster.write_file(f"{name}.sh", RESOURCE_PROBE + "sleep 300\n")
    out_path = f"{cluster.remote_dir}/{name}.out"
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-o", out_path]
        + sbatch_args + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    content = cluster.wait_output(out_path, "RESOURCE_PROBE_OK", timeout=120)
    assert "RESOURCE_PROBE_OK" in content, (
        f"probe never ran\n{cluster.debug_job(job_id)}\n{content}"
    )
    return job_id, key_values(content)


def _layout(cluster) -> tuple[int, set[int], int, int]:
    """(cores per GPU, reserved CPUs, GPU count, schedulable CPUs) of node 0."""
    out = cluster.scontrol_show_node(cluster.node_names[0])
    per_gpu, reserved = node_reserved_cpus(out)
    m = re.search(r"CPUEfctv=(\d+)", out)
    assert m, out
    return per_gpu, reserved, cluster.node_gpu_count(cluster.node_names[0]), int(m.group(1))


@pytest.mark.rootful
class TestGpuReservedCores:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes):
        return restricted_cores_config(ssh_nodes[0])

    def test_layout_is_shown_on_the_node(self, gpu_cluster, cluster_config_overrides):
        cluster = gpu_cluster
        per_gpu, reserved, gpus, _ = _layout(cluster)
        configured = cluster_config_overrides["nodes"][0]["restricted_cores_per_gpu"]
        assert per_gpu == configured
        threads = threads_per_core(cluster.nodes[0])
        assert len(reserved) == gpus * per_gpu * threads, (
            f"{gpus} GPUs x {per_gpu} cores x {threads} threads, got {sorted(reserved)}"
        )
        # Per GPU, not a NUMA-wide modulo: a node could reserve an uneven
        # share per NUMA node and still have every GPU's own cores whole.
        out = cluster.scontrol_show_node(cluster.node_names[0])
        numas = numa_cpus(cluster.nodes[0])
        gpu_lines = [line for line in out.splitlines() if line.strip().startswith("GPUDevice=")]
        assert gpu_lines, f"no GPUDevice lines:\n{out}"
        for line in gpu_lines:
            fields = dict(tok.split("=", 1) for tok in line.split() if "=" in tok)
            assert "ReservedCPUs" in fields, line
            mine = expand_cpulist(fields["ReservedCPUs"])
            assert len(mine) == per_gpu * threads, (
                f"{per_gpu} cores x {threads} threads, got {sorted(mine)}"
            )
            assert any(mine <= numa for numa in numas), (
                f"each GPU's cores come whole from one NUMA node: {sorted(mine)}"
            )

    def test_gpu_jobs_run_on_their_gpus_reserved_cpus(self, gpu_cluster):
        cluster = gpu_cluster
        _, reserved, gpus, _ = _layout(cluster)
        per_gpu_cpus = len(reserved) // gpus
        jobs, sets = [], []
        try:
            for i in range(min(gpus, 2)):
                job_id, values = _hold_probe(
                    cluster, f"rsv-gpu-{i}", ["--gres=gpu:1", "-c", str(per_gpu_cpus)]
                )
                jobs.append(job_id)
                sets.append(expand_cpulist(values["CPUS"]))
        finally:
            for job_id in jobs:
                cluster.scancel(job_id)

        numas = numa_cpus(cluster.nodes[0])
        for cpus in sets:
            assert len(cpus) == per_gpu_cpus and cpus <= reserved, (
                f"a GPU job's {per_gpu_cpus} CPUs are its GPU's reserved ones: "
                f"{sorted(cpus)} vs reserved {sorted(reserved)}"
            )
            assert any(cpus <= numa for numa in numas), (
                f"one GPU's reserved CPUs share a NUMA node: {sorted(cpus)}"
            )
        if len(sets) == 2:
            assert sets[0].isdisjoint(sets[1]), f"two GPUs, two sets: {sets}"

    def test_cpu_only_job_takes_only_the_general_pool(self, gpu_cluster):
        cluster = gpu_cluster
        _, reserved, _, schedulable = _layout(cluster)
        general = schedulable - len(reserved)
        job_id, values = _hold_probe(cluster, "rsv-cpu", ["-n", str(general)])
        cluster.scancel(job_id)

        cpus = expand_cpulist(values["CPUS"])
        assert len(cpus) == general, f"expected the {general}-CPU general pool: {values}"
        assert cpus.isdisjoint(reserved), (
            f"a CPU-only job never lands on GPU-reserved CPUs: {sorted(cpus & reserved)}"
        )

    def test_cpu_only_job_beyond_the_general_pool_pends(self, gpu_cluster):
        cluster = gpu_cluster
        _, reserved, _, schedulable = _layout(cluster)
        too_many = schedulable - len(reserved) + 1
        hold = cluster.write_file("rsv-big.sh", "#!/bin/bash\nsleep 300\n")
        sb = cluster.sbatch(
            ["-J", "rsv-big", "-N", "1", "-w", cluster.node_names[0], "-n", str(too_many), hold]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        try:
            wait_job_state(cluster, job_id, "PD", timeout=60)
            detail = wait_until(
                lambda: (out := cluster.scontrol("show", "job", str(job_id)))
                and "outside GPU-reserved" in out and out,
                timeout_s=60, interval_s=2,
                desc="a pending reason naming the GPU-reserved cores",
            )
        finally:
            cluster.scancel(job_id)
        assert f"needs {too_many} CPUs" in detail, detail

    def test_node_shows_cpu_topology_and_per_gpu_cpus(self, gpu_cluster):
        """S:C:T matches lscpu; each GPU line names its own reserved and nearby CPUs."""
        cluster = gpu_cluster
        out = cluster.scontrol_show_node(cluster.node_names[0])

        rows = [
            line.split(",")
            for line in cluster.nodes[0].exec("lscpu -p=CPU,CORE,SOCKET").splitlines()
            if line and not line.startswith("#")
        ]
        sockets = {sock for _, _, sock in rows}
        cores = {(sock, core) for _, core, sock in rows}
        expected = (len(sockets), len(cores) // len(sockets), len(rows) // len(cores))
        m = re.search(r"Sockets=(\d+) CoresPerSocket=(\d+) ThreadsPerCore=(\d+)", out)
        assert m, f"no S:C:T on the CPUTot line:\n{out}"
        assert tuple(map(int, m.groups())) == expected, f"lscpu gives {expected}:\n{out}"

        per_gpu, reserved = node_reserved_cpus(out)
        threads = expected[2]
        numas = numa_cpus(cluster.nodes[0])
        gpu_lines = [line for line in out.splitlines() if line.strip().startswith("GPUDevice=")]
        assert gpu_lines, f"no GPUDevice lines:\n{out}"
        seen: set[int] = set()
        for line in gpu_lines:
            fields = dict(tok.split("=", 1) for tok in line.split() if "=" in tok)
            assert "ReservedCPUs" in fields and "AffinityCPUs" in fields, line
            mine = expand_cpulist(fields["ReservedCPUs"])
            near = expand_cpulist(fields["AffinityCPUs"])
            assert len(mine) == per_gpu * threads, f"{per_gpu} cores x {threads}: {line}"
            assert mine <= near, f"picks come from the GPU's nearby CPUs: {line}"
            assert near in numas, f"no cores= is set, so nearby is one NUMA node: {line}"
            assert seen.isdisjoint(mine), f"two GPUs share reserved CPUs: {line}"
            seen |= mine
        assert seen == reserved, (
            f"per-GPU picks {sorted(seen)} must add up to RestrictedCoresPerGPU's {sorted(reserved)}"
        )
        assert re.search(r"^\s*Capabilities=\S*signal-job", out, re.M), out
