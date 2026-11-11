# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Job labels the GPU metrics exporter reads from the node prolog.

The prolog and epilog are the scripts the admin guide ships. They name a
file per GPU from SPUR_JOB_GPUS, and the exporter on the node (port 5002)
attaches those files' job id to the matching gpu_id.
"""

from __future__ import annotations

import shlex
import time

import pytest

from cluster import SpurCluster, job_state, parse_job_id, wait_job

EXPORT_DIR = "/var/run/exporter"

_PROLOG = """#!/bin/bash
EXPORT_DIR="__EXPORT__"
IFS=',' read -ra GPU_IDS <<< "${SPUR_JOB_GPUS}"
RENDER_IDS=()
for gpu in "${GPU_IDS[@]}"; do
    RENDER_IDS+=("$((gpu % 128))")
done
CUDA_VISIBLE_DEVICES=$(IFS=','; echo "${RENDER_IDS[*]}")
[ -d "${EXPORT_DIR}" ] || exit 0
for id in "${RENDER_IDS[@]}"; do
    [ -n "$id" ] || continue
    printf '{"SLURM_JOB_ID":"%s","SLURM_JOB_USER":"%s","SLURM_JOB_PARTITION":"%s","CUDA_VISIBLE_DEVICES":"%s"}' \\
        "${SLURM_JOB_ID}" "${SPUR_JOB_USER}" "${SLURM_JOB_PARTITION}" "${CUDA_VISIBLE_DEVICES}" \\
        > "${EXPORT_DIR}/${id}"
done
printf '%s\\n' "${SPUR_JOB_GPUS}" > "${EXPORT_DIR}/seen-${SLURM_JOB_ID}"
"""

_EPILOG = """#!/bin/bash
EXPORT_DIR="__EXPORT__"
IFS=',' read -ra GPU_IDS <<< "${SPUR_JOB_GPUS}"
[ -d "${EXPORT_DIR}" ] || exit 0
for gpu in "${GPU_IDS[@]}"; do
    [ -n "$gpu" ] || continue
    rm -f "${EXPORT_DIR}/$((gpu % 128))"
done
rm -f "${EXPORT_DIR}/seen-${SLURM_JOB_ID}"
"""

_TASK_PROLOG = """#!/bin/bash
printf '%s\\n' "$SPUR_JOB_GPUS" > "__DIR__/task-$SPUR_JOB_ID"
"""


def _sudo(node) -> str:
    if node.exec_allow_fail('test "$(id -u)" = 0 && echo root').strip() == "root":
        return ""
    return "sudo "


def _exporter_files(node) -> dict[str, str]:
    sudo = _sudo(node)
    listing = node.exec_allow_fail(f"{sudo}ls -1 {EXPORT_DIR}").strip()
    files = {}
    for name in listing.splitlines():
        if name.isdigit():
            files[name] = node.exec_allow_fail(f"{sudo}cat {EXPORT_DIR}/{name}")
    return files


def _seen(node, job_id: int) -> str:
    code, out = node.exec_with_exit(f"{_sudo(node)}cat {EXPORT_DIR}/seen-{job_id}")
    return "" if code != 0 else out.strip()


def _sudo_cat(node, path: str) -> str:
    return node.exec_allow_fail(f"{_sudo(node)}cat {path}").strip()


def _clear_exporter(node) -> None:
    sudo = _sudo(node)
    node.exec(f"{sudo}mkdir -p {EXPORT_DIR}")
    node.exec(f"{sudo}chmod 755 {EXPORT_DIR}")
    node.exec(f"{sudo}find {EXPORT_DIR} -maxdepth 1 -type f -delete")


def _metric_job_ids(node) -> dict[str, str]:
    import re

    raw = node.exec_allow_fail("curl -sf --max-time 5 localhost:5002/metrics")
    found = {}
    for body in re.findall(r"amd_gpu_gfx_activity\{([^}]*)\}", raw):
        gpu_id = re.search(r'gpu_id="([^"]*)"', body)
        job_id = re.search(r'job_id="([^"]*)"', body)
        if gpu_id and job_id:
            found[gpu_id.group(1)] = job_id.group(1)
    return found


def _wait_running(cluster: SpurCluster, job_id: int, timeout: int = 90) -> None:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        last = cluster.squeue_all()
        if job_state(last, job_id) == "R":
            return
        time.sleep(1)
    raise AssertionError(f"job {job_id} never ran:\n{last}")


def _install_hooks(cluster: SpurCluster) -> None:
    node = cluster.nodes[0]
    sudo = _sudo(node)
    prolog = cluster.write_file(
        "hooks/slurm-prolog.sh", _PROLOG.replace("__EXPORT__", EXPORT_DIR), all_nodes=True
    )
    epilog = cluster.write_file(
        "hooks/slurm-epilog.sh", _EPILOG.replace("__EXPORT__", EXPORT_DIR), all_nodes=True
    )
    task = cluster.write_file(
        "hooks/task-prolog.sh",
        _TASK_PROLOG.replace("__DIR__", cluster.remote_dir),
        all_nodes=True,
    )
    quoted = " ".join(shlex.quote(p) for p in (prolog, epilog, task))
    node.exec(f"{sudo}chown root:root {quoted}")
    node.exec(f"{sudo}chmod 755 {quoted}")
    as_root = cluster.agent_as_root
    cluster.stop()
    try:
        cluster.start(
            {
                **SpurCluster.devices_config(auto_detect=True),
                "hooks": {"prolog": prolog, "epilog": epilog, "task_prolog": task},
            },
            agent_as_root=as_root,
        )
    except Exception:
        print(cluster.spurd_log())
        print(cluster.spurctld_log()[-4000:])
        raise


def _indices(text: str) -> list[str]:
    return [part for part in text.split(",") if part]


@pytest.mark.rootful
def test_each_allocated_gpu_is_labelled_for_the_job_and_cleared_after(gpu_cluster):
    cluster = gpu_cluster
    cluster.gpu_preflight(1)
    node = cluster.nodes[0]
    _clear_exporter(node)
    _install_hooks(cluster)
    if cluster.node_gpu_count(cluster.node_names[0]) < 4:
        pytest.skip("need >= 4 GPUs")

    cpu = cluster.write_file("cpu.sh", "#!/bin/bash\ntrue\n")
    cpu_id = parse_job_id(cluster.sbatch(["-J", "cpu", "-N", "1", "-o", "/dev/null", cpu]))
    assert cpu_id is not None
    wait_job(cluster, cpu_id)
    assert _exporter_files(node) == {}

    hold = cluster.write_file(
        "hold.sh",
        "#!/bin/bash\n"
        "echo SPUR_JOB_GPUS=$SPUR_JOB_GPUS\n"
        "echo ROCR_VISIBLE_DEVICES=$ROCR_VISIBLE_DEVICES\n"
        "sleep 40\n"
        "echo DONE\n",
    )
    out = f"{cluster.remote_dir}/hold.out"
    job_id = parse_job_id(cluster.sbatch(
        ["-J", "labelled", "-N", "1", "--gres=gpu:4", "-o", out, hold]
    ))
    assert job_id is not None
    try:
        _wait_running(cluster, job_id)
        cluster.wait_output(out, "ROCR_VISIBLE_DEVICES=", timeout=30)
        job_env = cluster.nodes[0].read_file(out)
        visible = ""
        for line in job_env.splitlines():
            if line.startswith("ROCR_VISIBLE_DEVICES="):
                visible = line.split("=", 1)[1].strip()
        hook_gpus = _seen(node, job_id)
        task_gpus = _sudo_cat(node, f"{cluster.remote_dir}/task-{job_id}")
        files = _exporter_files(node)
        metrics = _metric_job_ids(node)
        report = f"hook={hook_gpus!r} task={task_gpus!r} visible={visible!r} files={sorted(files)} metrics={metrics}"
        print(report)
        indices = _indices(hook_gpus)
        assert indices == _indices(visible) == _indices(task_gpus), report
        assert len(indices) == 4 and all(int(g) < 128 for g in indices), report
        assert set(files) == set(indices), report
        for body in files.values():
            assert f'"SLURM_JOB_ID":"{job_id}"' in body, report
        if metrics:
            for gpu in indices:
                assert metrics.get(gpu) == str(job_id), report
            assert all(metrics[gpu] == "" for gpu in metrics if gpu not in indices), report

        wait_job(cluster, job_id, timeout=90)
        assert _exporter_files(node) == {}
        if metrics:
            assert all(value == "" for value in _metric_job_ids(node).values()), _metric_job_ids(node)

        first = parse_job_id(cluster.sbatch(
            ["-J", "a", "-N", "1", "--gres=gpu:1", "-o", "/dev/null", hold]
        ))
        second = parse_job_id(cluster.sbatch(
            ["-J", "b", "-N", "1", "--gres=gpu:1", "-o", "/dev/null", hold]
        ))
        assert first and second
        _wait_running(cluster, first)
        _wait_running(cluster, second)
        assert _seen(node, first) != _seen(node, second)
        assert set(_exporter_files(node)) == {_seen(node, first), _seen(node, second)}
        cluster.scancel(str(first))
        deadline = time.time() + 30
        while time.time() < deadline and _seen(node, first):
            time.sleep(1)
        assert _seen(node, second), "ending the first job removed the second job's label"
        assert _seen(node, first) == ""
        cluster.scancel(str(second))
        deadline = time.time() + 30
        while time.time() < deadline and _exporter_files(node):
            time.sleep(1)
        assert _exporter_files(node) == {}

        cmd = " ".join(
            cluster._cli_env_assignments()
            + ["nohup", shlex.quote(f"{cluster.bin_dir}/srun"), "-N", "1", "--gres=gpu:2", "sleep", "25"]
        )
        node.exec(f"{cmd} >{cluster.remote_dir}/srun.out 2>&1 &")
        deadline = time.time() + 40
        srun_seen = ""
        while time.time() < deadline:
            listing = node.exec_allow_fail(f"{_sudo(node)}ls -1 {EXPORT_DIR}").split()
            seen = [name for name in listing if name.startswith("seen-")]
            if seen:
                srun_seen = _sudo_cat(node, f"{EXPORT_DIR}/{seen[0]}")
                break
            time.sleep(1)
        srun_indices = _indices(srun_seen)
        if len(srun_indices) != 2:
            print(node.read_file(f"{cluster.remote_dir}/srun.out"))
        assert len(srun_indices) == 2 and all(int(g) < 128 for g in srun_indices), srun_seen
        assert set(_exporter_files(node)) == set(srun_indices)
        deadline = time.time() + 40
        while time.time() < deadline and _exporter_files(node):
            time.sleep(1)
        assert _exporter_files(node) == {}
    finally:
        _clear_exporter(node)
