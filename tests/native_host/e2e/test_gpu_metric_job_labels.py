# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""GPU metric job labels from the exporter hooks shipped in examples/hooks.

The node prolog writes one file per GPU of the job into /var/run/exporter,
named by the GPU's render ID and naming only that GPU; the epilog removes them.
The AMD Device Metrics Exporter turns those files into job labels on its GPU
metrics. The files are checked on every GPU node. The labels are checked too
when an exporter answers on the node.
"""

from __future__ import annotations

import json
import re
import shlex
import time
from pathlib import Path

import pytest

from cluster import SpurCluster, job_state, parse_job_id, wait_job

EXPORT_DIR = "/var/run/exporter"
HOOKS_DIR = Path(__file__).resolve().parents[3] / "examples" / "hooks"

_WRAPPER = """#!/bin/bash
export SPUR_HOOKS="__DIR__"
exec "__DIR__/__SCRIPT__"
"""

_HOLD = """#!/bin/bash
echo "DRI=$(ls /dev/dri | grep renderD | tr '\\n' ' ')"
sleep 300
"""


def _sudo(node) -> str:
    if node.exec_allow_fail('test "$(id -u)" = 0 && echo root').strip() == "root":
        return ""
    return "sudo "


def _exporter_files(node) -> dict[str, dict]:
    sudo = _sudo(node)
    files = {}
    for name in node.exec_allow_fail(f"{sudo}ls -1 {EXPORT_DIR}").split():
        if name.isdigit():
            body = node.exec_allow_fail(f"{sudo}cat {EXPORT_DIR}/{name}")
            files[name] = json.loads(body)
    return files


def _clear_exporter_dir(node) -> None:
    sudo = _sudo(node)
    node.exec(f"{sudo}mkdir -p {EXPORT_DIR}")
    node.exec(f"{sudo}find {EXPORT_DIR} -maxdepth 1 -type f -name '[0-9]*' -delete")


def _exporter_port(node) -> int:
    """The exporter's ServerPort from the config file it was started with."""
    cmdline = node.exec_allow_fail("pgrep -a amd-metrics-exp || true")
    match = re.search(r"-amd-metrics-config[= ](\S+)", cmdline)
    config = match.group(1) if match else "/etc/metrics/config.json"
    raw = node.exec_allow_fail(f"{_sudo(node)}cat {shlex.quote(config)} 2>/dev/null || true")
    try:
        return int(json.loads(raw).get("ServerPort") or 5000)
    except ValueError:
        return 5000


def _metric_job_ids(node, port: int) -> dict[str, str] | None:
    raw = node.exec_allow_fail(f"curl -sf --max-time 5 localhost:{port}/metrics || true")
    series = re.findall(r"gpu_gfx_activity\{([^}]*)\}", raw)
    if not series:
        return None
    found = {}
    for labels in series:
        gpu = re.search(r'gpu_id="([^"]*)"', labels)
        job = re.search(r'job_id="([^"]*)"', labels)
        if gpu:
            found[gpu.group(1)] = job.group(1) if job else ""
    return found


def _install_dme(cluster: SpurCluster) -> tuple[str, str]:
    paths = tuple(
        cluster.write_file(f"hooks/{name}", (HOOKS_DIR / "dme" / name).read_text(), all_nodes=True)
        for name in ("slurm-prolog.sh", "slurm-epilog.sh")
    )
    return paths[0], paths[1]


def _install_gpu_node(cluster: SpurCluster) -> tuple[str, str]:
    for name in ("common-env.sh", "hook-cleanup.sh", "exporter-prolog.sh", "exporter-epilog.sh"):
        cluster.write_file(f"gpu-node/{name}", (HOOKS_DIR / "gpu-node" / name).read_text(), all_nodes=True)
    hooks_dir = f"{cluster.remote_dir}/gpu-node"
    wrappers = []
    for hook in ("prolog", "epilog"):
        body = _WRAPPER.replace("__DIR__", hooks_dir).replace("__SCRIPT__", f"exporter-{hook}.sh")
        wrappers.append(cluster.write_file(f"gpu-node-{hook}.sh", body, all_nodes=True))
    return wrappers[0], wrappers[1]


INSTALLERS = {"dme": _install_dme, "gpu-node": _install_gpu_node}


def _start_with_hooks(cluster: SpurCluster, prolog: str, epilog: str) -> None:
    root = shlex.quote(cluster.remote_dir)
    for node in cluster.nodes:
        node.exec(f"{_sudo(node)}chown -R root:root {root}/hooks {root}/gpu-node {root}/gpu-node-*.sh "
                  "2>/dev/null || true")
        node.exec(f"{_sudo(node)}chown root:root {shlex.quote(prolog)} {shlex.quote(epilog)}")
    as_root = cluster.agent_as_root
    cluster.stop()
    cluster.start(
        {
            **SpurCluster.devices_config(auto_detect=True),
            "hooks": {"prolog": prolog, "epilog": epilog},
        },
        agent_as_root=as_root,
    )


def _wait_running(cluster: SpurCluster, job_id: int, timeout: int = 90) -> None:
    deadline = time.time() + timeout
    queue = ""
    while time.time() < deadline:
        queue = cluster.squeue_all()
        if job_state(queue, job_id) == "R":
            return
        time.sleep(1)
    raise AssertionError(f"job {job_id} never ran:\n{queue}")


def _render_ids_seen_by(cluster: SpurCluster, out: str) -> set[str]:
    text = cluster.wait_output(out, "DRI=", timeout=60)
    match = re.search(r"DRI=(.*)", text)
    assert match, f"no DRI= line in {out}:\n{text}"
    return {name.removeprefix("renderD") for name in match.group(1).split()}


class _LabelCheck:
    """Waits for the exporter files, and labels if an exporter runs, to match the jobs."""

    def __init__(self, node, port: int | None):
        self.node = node
        self.port = port

    def expect(self, step: str, jobs: dict[int, set[str]], timeout: int = 30) -> None:
        want = {render: str(job) for job, renders in jobs.items() for render in renders}
        want_counts = {str(job): len(renders) for job, renders in jobs.items()}
        deadline = time.time() + timeout
        while True:
            files = _exporter_files(self.node)
            owners = {name: body.get("SLURM_JOB_ID") for name, body in files.items()}
            scopes = {name: body.get("CUDA_VISIBLE_DEVICES") for name, body in files.items()}
            labels = _metric_job_ids(self.node, self.port) if self.port else None
            counts = {}
            for job in (labels or {}).values():
                if job:
                    counts[job] = counts.get(job, 0) + 1
            files_ok = owners == want and all(scopes[name] == name for name in files)
            labels_ok = labels is None or counts == want_counts
            if (files_ok and labels_ok) or time.time() > deadline:
                break
            time.sleep(1)
        print(f"step={step} want={want} files={owners} scopes={scopes} labels={labels}", flush=True)
        assert owners == want, f"{step}: exporter files {owners}, want {want}"
        for name, scope in scopes.items():
            assert scope == name, f"{step}: file {name} applies to GPUs {scope!r}, not only itself"
        if labels is not None:
            assert counts == want_counts, f"{step}: labelled GPUs per job {counts}, want {want_counts}"


@pytest.mark.rootful
@pytest.mark.parametrize("scripts", sorted(INSTALLERS))
def test_each_gpu_is_labelled_for_its_job_and_cleared_after(gpu_cluster, scripts):
    cluster = gpu_cluster
    cluster.gpu_preflight(1)
    node = cluster.nodes[0]
    if cluster.node_gpu_count(cluster.node_names[0]) < 4:
        pytest.skip("needs a node with at least 4 GPUs")
    _clear_exporter_dir(node)
    prolog, epilog = INSTALLERS[scripts](cluster)
    _start_with_hooks(cluster, prolog, epilog)
    port = _exporter_port(node)
    exporter = port if _metric_job_ids(node, port) is not None else None
    print(f"scripts={scripts} exporter_port={exporter}", flush=True)
    check = _LabelCheck(node, exporter)
    hold = cluster.write_file("hold.sh", _HOLD)

    def submit(name: str, gpus: int) -> tuple[int, set[str]]:
        out = f"{cluster.remote_dir}/{name}.out"
        job = parse_job_id(cluster.sbatch(["-J", name, "-N", "1", f"--gres=gpu:{gpus}", "-o", out, hold]))
        assert job is not None
        _wait_running(cluster, job)
        renders = _render_ids_seen_by(cluster, out)
        print(f"job {job} ({name}) holds renderD{sorted(renders)}", flush=True)
        assert len(renders) == gpus
        return job, renders

    try:
        check.expect("idle", {})

        four, four_gpus = submit("four", 4)
        check.expect("4gpu-running", {four: four_gpus})
        cluster.scancel(str(four))
        wait_job(cluster, four, timeout=60)
        check.expect("4gpu-ended", {})

        a, a_gpus = submit("a", 1)
        b, b_gpus = submit("b", 1)
        assert a_gpus != b_gpus
        check.expect("two-1gpu-running", {a: a_gpus, b: b_gpus})
        cluster.scancel(str(a))
        wait_job(cluster, a, timeout=60)
        assert job_state(cluster.squeue_all(), b) == "R"
        check.expect("first-cancelled-second-running", {b: b_gpus})
        cluster.scancel(str(b))
        wait_job(cluster, b, timeout=60)
        check.expect("both-ended", {})

        out = f"{cluster.remote_dir}/srun.out"
        srun_hold = cluster.write_file("srun-hold.sh", _HOLD.replace("300", "60"))
        srun = " ".join(
            cluster._cli_env_assignments()
            + ["nohup", shlex.quote(f"{cluster.bin_dir}/srun"), "-N", "1", "--gres=gpu:2", srun_hold]
        )
        node.exec(f"{srun} >{out} 2>&1 &")
        srun_gpus = _render_ids_seen_by(cluster, out)
        print(f"srun step holds renderD{sorted(srun_gpus)}: {node.read_file(out).strip()!r}", flush=True)
        assert len(srun_gpus) == 2
        queue = cluster.squeue_all()
        srun_job = next(job for job in range(b + 1, b + 4) if job_state(queue, job) == "R")
        check.expect("srun-running", {srun_job: srun_gpus})
        check.expect("srun-ended", {}, timeout=120)
    finally:
        _clear_exporter_dir(node)
