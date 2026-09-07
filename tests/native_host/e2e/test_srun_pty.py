# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `srun --pty` inside an allocation.

The command must run through an interactive PTY session rather than the
buffered RunStep path, and the step it creates must reach a terminal state.
"""


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
