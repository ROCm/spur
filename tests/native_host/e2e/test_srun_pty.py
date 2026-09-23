# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `srun --pty` inside an allocation.

The command must run through an interactive PTY session rather than the
buffered RunStep path, and the step it creates must reach a terminal state.
"""

import re
import threading
import time

# Below this, a step id names a reserved (batch/extern/interactive) session
# that every allocation already gets a supervisor for; a bare `pgrep
# spurstepd` can't tell that apart from the one this test is actually about.
_RESERVED_STEP_FLOOR = 0xFFFFFFF0


def _user_step_sessions_for_job(cluster, job_id: int, node_index: int = 0) -> set[str]:
    node = cluster.nodes[node_index]
    listing = node.exec_allow_fail(
        f"ls '{cluster.state_dir}/runtime' 2>/dev/null || true"
    )
    prefix = f"{job_id}."
    sessions = set()
    for name in listing.split():
        if not name.startswith(prefix):
            continue
        try:
            step_id = int(name.rsplit(".", 1)[-1])
        except ValueError:
            continue
        if step_id < _RESERVED_STEP_FLOOR:
            sessions.add(name)
    return sessions


def _reserved_step_sessions_for_job(cluster, job_id: int, node_index: int = 0) -> set[str]:
    node = cluster.nodes[node_index]
    listing = node.exec_allow_fail(
        f"ls '{cluster.state_dir}/runtime' 2>/dev/null || true"
    )
    prefix = f"{job_id}."
    sessions = set()
    for name in listing.split():
        if not name.startswith(prefix):
            continue
        try:
            step_id = int(name.rsplit(".", 1)[-1])
        except ValueError:
            continue
        if step_id >= _RESERVED_STEP_FLOOR:
            sessions.add(name)
    return sessions


def _assert_ticks_not_replayed(out: str, ticks: int = 20) -> None:
    """A shell relaunched from scratch after a restart repeats its early
    ticks; a reconnect racing the restart only drops a line or two of a
    terminal it can't buffer while disconnected. Duplicates, not gaps,
    are what distinguish a restart from a clean reconnect.
    """
    duplicated = [i for i in range(1, ticks + 1) if out.count(f"tick {i}\r\n") > 1]
    assert not duplicated, f"tick(s) {duplicated} repeated — the shell was restarted:\n{out}"


class TestSrunPtyStep:
    def test_pty_step_runs_on_a_tty(self, cluster):
        # The agent allocates the PTY, so `test -t 1` holds only on the
        # interactive path — the buffered path hands the task a pipe.
        code, out = cluster.salloc_run(
            "srun --pty bash -c 'test -t 1 && echo IS-TTY || echo NO-TTY'\n"
        )
        assert code == 0, out
        assert "IS-TTY" in out, out
        assert "NO-TTY" not in out, out

    def test_non_pty_step_does_not_get_a_tty(self, cluster):
        code, out = cluster.salloc_run(
            "srun bash -c 'test -t 1 && echo IS-TTY || echo NO-TTY'\n"
        )
        assert code == 0, out
        assert "NO-TTY" in out, out
        assert "IS-TTY" not in out, out

    def test_pty_step_reaches_terminal_state(self, cluster):
        # Nothing else closes an interactive step, so without the client
        # reporting back it stays RUNNING for the life of the allocation.
        code, out = cluster.salloc_run(
            'srun --pty bash -c "exit 9" || echo "pty-exit=$?"\n'
            # The allocation's own batch step stays RUNNING; only the pty step
            # is under test, so filter it out.
            'scontrol show step "$SPUR_JOB_ID" | grep -v "StepId=[0-9]*\\.batch"\n'
        )
        assert code == 0, out
        assert "pty-exit=9" in out, out
        assert "State=FAILED" in out, out
        assert "State=RUNNING" not in out, out

    def test_pty_step_feeds_derived_exit_code(self, cluster):
        code, out = cluster.salloc_run(
            "srun --pty bash -c 'exit 7' || true\n"
            'scontrol show job "$SPUR_JOB_ID" | grep -o "DerivedExitCode=[0-9:]*"\n'
        )
        assert code == 0, out
        assert "DerivedExitCode=7:0" in out, out

    def test_pty_no_longer_warns_that_container_is_dropped(self, cluster):
        # --pty now honors --container-image (see test_srun_container.py for the
        # in-container assertions); the old "not honored" warning must be gone. A
        # nonexistent image errors during setup rather than warning, so `|| true`
        # keeps the pipefail shell going.
        code, out = cluster.salloc_run(
            "srun --pty --container-image /nonexistent.sqsh echo hi || true\n"
        )
        assert code == 0, out
        assert "container options are not honored for a --pty step" not in out, out

    def test_pty_step_sizing_warning_tracks_what_was_typed(self, cluster):
        # salloc exports SPUR_NTASKS into every step, so a bare `srun --pty`
        # must stay quiet while an explicit -n still warns.
        sized = ["-N", "1", "-n", "2", "-t", "0:05"]
        code, out = cluster.salloc_run("srun --pty echo sized\n", salloc_args=sized)
        assert code == 0, out
        assert "are ignored" not in out, out

        code, out = cluster.salloc_run("srun --pty -n 2 echo sized\n", salloc_args=sized)
        assert code == 0, out
        assert "single task on one node" in out, out

    def test_pty_step_warns_on_dropped_io_and_env_flags(self, cluster):
        code, out = cluster.salloc_run(
            "srun --pty -o /tmp/spur-pty-out.txt --chdir /tmp --label echo hi\n"
        )
        assert code == 0, out
        assert "--output/--error are not applied" in out, out
        assert "--chdir does not move a --pty step" in out, out
        assert "--cpu-bind/--gpu-bind/--label/--mpi are ignored" in out, out

    def test_step_warnings_are_emitted_exactly_once(self, cluster):
        # The pty path chains both helpers and returns before dispatch_step, so
        # it is the only one that could double up.
        for flags in ("--pty", ""):
            code, out = cluster.salloc_run(
                f"srun {flags} --input /dev/null echo hi\n"
            )
            assert code == 0, out
            assert out.count("--input is not supported for a job step") == 1, out

    def test_pty_step_runs_srun_epilog(self, cluster):
        epilog = cluster.write_file(
            "pty-epilog.sh", "#!/bin/bash\ntouch /tmp/spur-pty-epilog-marker\n"
        )
        code, out = cluster.salloc_run(
            "rm -f /tmp/spur-pty-epilog-marker\n"
            f"srun --pty --epilog {epilog} true\n"
            "test -f /tmp/spur-pty-epilog-marker && echo EPILOG-RAN || echo NO-EPILOG\n"
        )
        assert code == 0, out
        assert "EPILOG-RAN" in out, out


class TestSrunPtyStepMultiNode:
    def test_pty_step_runs_on_the_requested_node(self, multi_node_cluster):
        # step_dispatch_kind threads -w into CreateJobStep; without it the
        # controller falls back to the first allocated node.
        cluster = multi_node_cluster
        first, target = cluster.node_names[0], cluster.node_names[1]
        assert first != target, "fixture must provide two distinct nodes"
        # Pin the allocation to both nodes: a bare -N 2 is a node count, so the
        # scheduler may exclude `target` and the step's -w below would then fail.
        # Tagged: salloc echoes the whole nodelist, so a bare name match would
        # hold even if the step ran on the wrong node.
        code, out = cluster.salloc_run(
            f'srun --pty -w {target} bash -c \'echo NODE=${{SPUR_TARGET_NODE:-$(hostname)}}\'\n',
            salloc_args=["-N", "2", "-w", f"{first},{target}", "-t", "0:05"],
        )
        assert code == 0, out
        hosts = {ln.split("=", 1)[1].strip() for ln in out.splitlines() if ln.startswith("NODE=")}
        assert hosts == {target}, out


class TestSrunPtyStepSupervision:
    """A `--pty` step used to run as a bare child of spurd, with no supervisor
    to outlive an agent restart. It now gets one, like any other step."""

    def test_pty_step_survives_an_agent_restart(self, cluster):
        result: dict[str, object] = {}
        job_name = f"pty-restart-survival-{time.time_ns()}"
        # Pinned: the session check and the restart both target node 0, so an
        # unpinned allocation could land elsewhere and make both a no-op.
        node = cluster.node_names[0]

        def run():
            result["code"], result["out"] = cluster.salloc_run(
                "srun --pty bash -c '"
                "for i in $(seq 1 20); do echo tick $i; sleep 1; done; "
                "echo SURVIVED'\n",
                salloc_args=["-N", "1", "-w", node, "-t", "0:05", "-J", job_name],
            )

        thread = threading.Thread(target=run)
        thread.start()
        try:
            deadline = time.time() + 30
            job_id = None
            while time.time() < deadline:
                ids = cluster.running_job_ids_by_name(job_name)
                if ids:
                    job_id = ids[0]
                    break
                time.sleep(1)
            assert job_id, "expected the allocation to appear before the restart"

            before: set[str] = set()
            while time.time() < deadline:
                # A bare (non-container) --pty step has no numbered-step
                # supervisor of its own — only the shared terminal placeholder.
                before = _reserved_step_sessions_for_job(cluster, job_id)
                if before:
                    break
                time.sleep(1)
            assert before, "expected a session for the pty step before the restart"

            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)

            # Identity, not count: a supervisor killed with the agent and
            # respawned afterwards would satisfy any "still one running" check.
            after = _reserved_step_sessions_for_job(cluster, job_id)
            assert before <= after, (
                "the pty step's supervisor must outlive the agent that spawned it: "
                f"{sorted(before)} before the restart, {sorted(after)} after"
            )
        finally:
            thread.join(timeout=60)

        assert result.get("code") == 0, result.get("out")
        out = str(result.get("out"))
        assert "SURVIVED" in out, out
        _assert_ticks_not_replayed(out)

    def test_interactive_session_ending_does_not_end_the_allocation(self, cluster):
        # salloc's own $SHELL runs on the client and keeps going regardless,
        # so check the job's own state from inside it, not just shell output.
        code, out = cluster.salloc_run(
            "srun --pty bash -c 'echo hi-from-pty; sleep 2'\n"
            "echo AFTER_PTY\n"
            'scontrol show job "$SPUR_JOB_ID" | grep -o "JobState=[A-Z]*"\n'
            "sleep 5\n"
            "echo END_OF_SHELL\n"
        )
        assert code == 0, out
        assert "AFTER_PTY" in out, (
            f"the allocation ended when the interactive session did:\n{out}"
        )
        assert "JobState=RUNNING" in out, (
            f"the allocation's own workload was killed off by the terminal session:\n{out}"
        )
        assert "END_OF_SHELL" in out, (
            f"the allocation's own shell did not survive to its own natural end:\n{out}"
        )

    def test_standalone_pty_srun_does_not_run_the_command_twice(self, cluster):
        # The interactive session runs the real command; the job's own
        # placeholder used to run it a second, unwatched time in its own
        # redundant container/environment.
        marker = f"{cluster.remote_dir}/pty-once-marker.txt"
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02", "--pty",
            "bash", "-c", f"echo ran >> '{marker}'",
        ])
        assert code == 0, out
        content = cluster.read_output_on_any_node(marker)
        assert content.count("ran") == 1, (
            f"the command must run exactly once, not once per placeholder:\n{content}"
        )
        job_id = re.search(r"Pending job allocation (\d+)", out)
        assert job_id, out
        job_info = cluster.scontrol("show", "job", job_id.group(1))
        assert "Command=sleep infinity" in job_info, (
            f"the job's own placeholder must not carry the real command:\n{job_info}"
        )

    def test_standalone_pty_srun_job_survives_an_agent_restart(self, cluster):
        # Standalone `srun --pty` (no salloc) submits its own job through the
        # same LaunchJob path as sbatch; that job's own supervisor must be
        # just as durable, not skipped because the job happens to be --pty.
        result: dict[str, object] = {}
        job_name = f"standalone-pty-restart-{time.time_ns()}"
        node = cluster.node_names[0]

        def run():
            result["code"], result["out"] = cluster.srun_with_exit([
                "-N", "1", "-w", node, "-t", "0:05", "-J", job_name, "--pty",
                "bash", "-c",
                "for i in $(seq 1 20); do echo tick $i; sleep 1; done; echo SURVIVED",
            ])

        thread = threading.Thread(target=run)
        thread.start()
        try:
            deadline = time.time() + 30
            job_id = None
            while time.time() < deadline:
                ids = cluster.running_job_ids_by_name(job_name)
                if ids:
                    job_id = ids[0]
                    break
                time.sleep(1)
            assert job_id, "expected the standalone job to appear before the restart"

            before: set[str] = set()
            while time.time() < deadline:
                before = _reserved_step_sessions_for_job(cluster, job_id)
                if before:
                    break
                time.sleep(1)
            assert before, "expected the job's own supervisor before the restart"

            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)

            after = _reserved_step_sessions_for_job(cluster, job_id)
            assert before <= after, (
                "a standalone --pty job's own supervisor must outlive the agent "
                f"that spawned it: {sorted(before)} before, {sorted(after)} after"
            )
        finally:
            thread.join(timeout=60)

        assert result.get("code") == 0, result.get("out")
        out = str(result.get("out"))
        assert "SURVIVED" in out, out
        _assert_ticks_not_replayed(out)

    def test_srun_pty_reconnects_after_the_agent_restarts(self, cluster):
        # A dropped mid-session stream (the agent restarting) must be
        # retried by the CLI, not reported as if the workload itself failed.
        result: dict[str, object] = {}
        job_name = f"pty-reconnect-{time.time_ns()}"
        node = cluster.node_names[0]

        def run():
            result["code"], result["out"] = cluster.salloc_run(
                "srun --pty bash -c '"
                "for i in $(seq 1 20); do echo tick $i; sleep 1; done; "
                "echo SURVIVED'\n",
                salloc_args=["-N", "1", "-w", node, "-t", "0:05", "-J", job_name],
            )

        thread = threading.Thread(target=run)
        thread.start()
        try:
            deadline = time.time() + 30
            job_id = None
            while time.time() < deadline:
                ids = cluster.running_job_ids_by_name(job_name)
                if ids:
                    job_id = ids[0]
                    break
                time.sleep(1)
            assert job_id, "expected the allocation to appear before the restart"

            before: set[str] = set()
            while time.time() < deadline:
                before = _reserved_step_sessions_for_job(cluster, job_id)
                if before:
                    break
                time.sleep(1)
            assert before, "expected a session for the pty step before the restart"

            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)
        finally:
            thread.join(timeout=60)

        assert result.get("code") == 0, result.get("out")
        out = str(result.get("out"))
        assert "reconnecting" in out, f"the client must report the dropped stream:\n{out}"
        assert "SURVIVED" in out, out
        _assert_ticks_not_replayed(out)
