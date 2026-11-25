# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for where a multi-node batch script runs (``--script-placement``)."""

import random
import re
import shlex
import time

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


def _step_state(cluster, job_id: int, step_id: int) -> str | None:
    out = cluster.scontrol("show", "step", str(job_id))
    match = re.search(rf"StepId={job_id}\.{step_id}\s.*?\bState=(\S+)", out)
    return match.group(1) if match else None


def _runs_anywhere(cluster, pattern: str) -> bool:
    """Whether a process matching *pattern* runs on any node. Bracket its first
    character so pgrep does not match the probe's own command line."""
    probe = f"pgrep -f {shlex.quote(pattern)} || true"
    return any(node.exec_allow_fail(probe).strip() for node in cluster.nodes)


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


class TestPerNodeLifecycle:
    def test_a_job_keeps_running_while_its_copies_finish_at_different_times(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        step_marker = f"{cluster.remote_dir}/late-step.ran"
        # Only node 0 is still running when its step starts, so the step lands there.
        script, marker = _runner(
            cluster,
            "staggered",
            '[ "$SLURM_NODEID" = 0 ] || exit 0\n'
            "sleep 15\n"
            f"srun -N 1 -n 1 bash -c 'hostname >> {step_marker}'",
        )

        job_id = _submit(cluster, script, "--script-placement=all-nodes")

        assert wait_job(cluster, job_id, timeout=180) == "CD", cluster.debug_job(job_id)
        assert len(_marker_lines(cluster, marker)) == NODES
        assert len(_marker_lines(cluster, step_marker)) == 1, (
            "a step must still run after another node's copy finished"
        )

    def test_the_first_failed_copy_stops_the_others_and_decides_the_exit(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        cluster.require_nodes(NODES)
        script, _ = _runner(
            cluster,
            "first-failure",
            '[ "$SLURM_NODEID" = 0 ] || exit 3\nsleep 600',
        )
        started = time.time()

        job_id = _submit(cluster, script, "--script-placement=all-nodes")

        assert wait_job(cluster, job_id, timeout=180) == "F", cluster.debug_job(job_id)
        assert time.time() - started < 300, "the sleeping copy must have been stopped"
        assert _show(cluster, job_id, "ExitCode") == "3:0"


class TestStepOnAFinishedCopysNode:
    # Pinned so the bound below holds whatever the harness defaults to.
    COMPLETE_WAIT = 600
    KILL_WAIT = 5

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "scheduler": {
                "complete_wait_secs": self.COMPLETE_WAIT,
                "kill_wait_secs": self.KILL_WAIT,
            }
        }

    def test_a_failure_stops_a_step_left_on_a_finished_copys_node(
        self, multi_node_cluster
    ):
        self._fail_node_1_over_a_step_left_on_node_0(multi_node_cluster, "kept-step", "")

    def test_a_step_whose_srun_died_ends_with_the_run(self, multi_node_cluster):
        cluster = multi_node_cluster
        # SIGKILL leaves srun no chance to abandon the step or record its end.
        self._fail_node_1_over_a_step_left_on_node_0(cluster, "orphan-step", "  kill -9 $!\n")

        drained = [
            name
            for name in cluster.node_names
            if "job cleanup was not confirmed" in cluster.scontrol_show_node(name)
        ]
        assert not drained, f"the run waited for the completing timeout: {drained}"

    def _fail_node_1_over_a_step_left_on_node_0(self, cluster, name: str, before_exit: str):
        cluster.require_nodes(NODES)
        # Unique per run, so the process check cannot see another run's step.
        step_sleep = random.randrange(10_000_000, 20_000_000)
        step_marker = f"{cluster.remote_dir}/{name}.step"
        copy_done = f"{cluster.remote_dir}/{name}.done"
        go = f"{cluster.remote_dir}/{name}.go"
        # Node 0 leaves a step on itself, as a one-node step lands on the first node
        # still running, and exits cleanly. When told to, node 1 waits until the
        # controller refuses node 0 new steps, so it holds node 0 only for the step,
        # checks the step still runs, and fails.
        script, _ = _runner(
            cluster,
            name,
            'if [ "$SLURM_NODEID" = 0 ]; then\n'
            "  srun -N 1 -n 1 bash -c "
            f"'hostname >> {step_marker}; exec sleep {step_sleep}' &\n"
            f"  until [ -s {step_marker} ]; do sleep 1; done\n"
            f"{before_exit}"
            f"  touch {copy_done}\n"
            "  exit 0\n"
            "fi\n"
            f"until [ -e {go} ]; do sleep 1; done\n"
            'node0=$(scontrol show job "$SLURM_JOB_ID" | sed -n "s/.*BatchHost=\\([^ ]*\\).*/\\1/p")\n'
            'until srun -N 1 -n 1 -w "$node0" true 2>&1 | grep -q "already finished its part"; do\n'
            "  sleep 1\n"
            "done\n"
            'scontrol show step "$SLURM_JOB_ID" | grep -q "StepId=$SLURM_JOB_ID\\.0 .*State=RUNNING" || exit 7\n'
            "exit 3",
        )
        pattern = f"[s]leep {step_sleep}"
        try:
            job_id = _submit(cluster, script, "--script-placement=all-nodes")

            deadline = time.time() + 120
            while not (
                _nodes_with_file(cluster, copy_done)
                and _step_state(cluster, job_id, 0) == "RUNNING"
            ):
                assert time.time() < deadline, cluster.debug_job(job_id)
                time.sleep(2)
            for node in cluster.nodes:
                node.exec(f"touch {shlex.quote(go)}")

            # Far under COMPLETE_WAIT, so only the stop can end the run in time.
            assert wait_job(cluster, job_id, timeout=120) == "F", cluster.debug_job(job_id)
            assert _show(cluster, job_id, "ExitCode") == "3:0", (
                "7 means the step did not outlive its copy"
            )
            assert len(_marker_lines(cluster, step_marker)) == 1, "the step must run on one node"
            assert _step_state(cluster, job_id, 0) not in (None, "PENDING", "RUNNING")
            deadline = time.time() + 30
            while _runs_anywhere(cluster, pattern) and time.time() < deadline:
                time.sleep(2)
            assert not _runs_anywhere(cluster, pattern), "the agent must have stopped the step"
        finally:
            for node in cluster.nodes:
                node.exec_allow_fail(f"pkill -f {shlex.quote(pattern)} || true")


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
