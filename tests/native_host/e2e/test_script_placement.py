# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for where a multi-node batch script runs (``--script-placement``)."""

import re
import shlex

import pytest

from cluster import parse_job_id, wait_job

NODES = 2


def _short(host: str) -> str:
    return host.strip().split(".")[0]


def _marker_lines(cluster, marker: str) -> list[str]:
    """Every line the nodes appended to their own copy of *marker*: no shared storage."""
    return [
        line
        for line in cluster.read_output_all_nodes(marker).splitlines()
        if line.strip()
    ]


def _runner(cluster, name: str, body: str) -> tuple[str, str]:
    """A batch script that records each node it runs on, then runs *body*."""
    marker = f"{cluster.remote_dir}/{name}.ran"
    script = cluster.write_file(
        f"{name}.sh", f"#!/bin/bash\nhostname >> {marker}\n{body}\n"
    )
    return script, marker


def _output(cluster, job_id: int) -> str:
    return f"{cluster.remote_dir}/placement-{job_id}.out"


def _submit(cluster, script: str, *flags: str) -> int:
    out = cluster.sbatch(
        [
            "-N",
            str(NODES),
            "--chdir",
            cluster.remote_dir,
            "-o",
            f"{cluster.remote_dir}/placement-%j.out",
            *flags,
            script,
        ]
    )
    job_id = parse_job_id(out)
    assert job_id is not None, out
    return job_id


def _show(cluster, job_id: int, key: str) -> str | None:
    match = re.search(rf"\b{key}=(\S+)", cluster.scontrol("show", "job", str(job_id)))
    return match.group(1) if match else None


def _nodes_with_file(cluster, path: str) -> int:
    probe = f"test -e {shlex.quote(path)} && echo present"
    return sum("present" in node.exec_allow_fail(probe) for node in cluster.nodes)


class TestScriptPlacement:
    def test_batch_host_runs_a_script_with_a_hidden_launcher_once(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        step_marker = f"{cluster.remote_dir}/hidden-step.ran"
        # The launcher sits one level down, where the `auto` text scan cannot see it.
        inner = cluster.write_file(
            "hidden-inner.sh",
            f"#!/bin/bash\nsrun -N {NODES} -n {NODES} bash -c 'hostname >> {step_marker}'\n",
            all_nodes=True,
        )
        script, marker = _runner(cluster, "hidden", f"bash {inner}")

        job_id = _submit(cluster, script, "--script-placement=batch-host")

        assert wait_job(cluster, job_id, timeout=180) == "CD", cluster.debug_job(job_id)
        ran = _marker_lines(cluster, marker)
        assert len(ran) == 1, f"the script must run once, ran on {ran}"
        steps = {_short(host) for host in _marker_lines(cluster, step_marker)}
        assert len(steps) == NODES, f"the step must reach every node: {steps}"
        assert _show(cluster, job_id, "ScriptPlacement") == "batch-host"
        assert _short(_show(cluster, job_id, "BatchHost") or "") == _short(ran[0])
        assert _nodes_with_file(cluster, _output(cluster, job_id)) == 1, (
            "a holding node must not open the job's output file"
        )

    def test_auto_runs_a_script_without_a_visible_launcher_on_every_node(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        script, marker = _runner(cluster, "plain", "true")

        job_id = _submit(cluster, script)

        assert wait_job(cluster, job_id, timeout=180) == "CD", cluster.debug_job(job_id)
        assert len(_marker_lines(cluster, marker)) == NODES
        assert _show(cluster, job_id, "ScriptPlacement") == "all-nodes"

    def test_the_environment_variable_needs_no_directive(self, multi_node_cluster):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        script, marker = _runner(cluster, "from-env", "true")

        cluster.cli_env["SBATCH_SCRIPT_PLACEMENT"] = "batch-host"
        try:
            job_id = _submit(cluster, script)
        finally:
            del cluster.cli_env["SBATCH_SCRIPT_PLACEMENT"]

        assert wait_job(cluster, job_id, timeout=180) == "CD", cluster.debug_job(job_id)
        assert len(_marker_lines(cluster, marker)) == 1

    def test_all_nodes_runs_a_script_that_names_srun_on_every_node(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        # `auto` reads `srun` here and would run the script on the batch host only.
        script, marker = _runner(cluster, "per-node", "if false; then srun hostname; fi")

        job_id = _submit(cluster, script, "--script-placement=all-nodes")

        assert wait_job(cluster, job_id, timeout=180) == "CD", cluster.debug_job(job_id)
        assert len(_marker_lines(cluster, marker)) == NODES


class TestClusterDefaultPlacement:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"script_placement": "batch-host"}}

    def test_a_job_that_does_not_choose_takes_the_cluster_default(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        script, marker = _runner(cluster, "defaulted", "true")

        job_id = _submit(cluster, script)

        assert wait_job(cluster, job_id, timeout=180) == "CD", cluster.debug_job(job_id)
        assert len(_marker_lines(cluster, marker)) == 1
        assert _show(cluster, job_id, "ScriptPlacement") == "batch-host"
