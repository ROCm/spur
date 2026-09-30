# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for srun step output delivery (spur#781).

A command run as an srun step inside an allocation has its stdout/stderr
redirected to per-step spool files on the node; the client tails those files
live via StreamJobOutput while the RunStep dispatch is in flight. These tests
drive real steps through an allocation shell and assert their output reaches
the client intact.
"""

# Mirrors MAX_STEP_NAME_LEN in crates/spur-core/src/step.rs.
_MAX_STEP_NAME_LEN = 256


class TestSrunStepOutput:
    def test_step_stdout_reaches_client(self, cluster):
        code, out = cluster.salloc_run(
            'srun echo STEP-MARKER-ALPHA\n'
            'srun bash -c "echo line1; echo line2; echo line3"\n'
        )
        assert code == 0, out
        assert "STEP-MARKER-ALPHA" in out, out
        for line in ("line1", "line2", "line3"):
            assert line in out, out

    def test_step_stderr_reaches_client(self, cluster):
        code, out = cluster.salloc_run(
            'srun bash -c "echo to-stderr 1>&2"\n'
        )
        assert code == 0, out
        assert "to-stderr" in out, out

    def test_step_exit_code_and_output_both_delivered(self, cluster):
        # srun exits with the step's exit code; the output before the failure
        # must still reach the client through the streaming path.
        code, out = cluster.salloc_run(
            'srun bash -c "echo before-fail; exit 7" || echo "step-exit=$?"\n'
        )
        assert code == 0, out
        assert "before-fail" in out, out
        assert "step-exit=7" in out, out

    def test_long_command_line_does_not_become_the_step_name(self, cluster):
        # The controller stores the argv as the step name, so an unbounded one
        # is replicated into every Raft entry and snapshot.
        code, out = cluster.salloc_run(
            'srun bash -c "echo LONG-ARGV-DONE; true $(head -c 20000 /dev/zero | tr \'\\0\' x)"\n'
            'scontrol show step "$SPUR_JOB_ID" | grep -v "StepId=[0-9]*\\.batch"\n'
        )
        assert code == 0, out
        assert "LONG-ARGV-DONE" in out, out

        step_lines = [ln for ln in out.splitlines() if "StepName=" in ln]
        assert step_lines, out
        for line in step_lines:
            name = line.split("StepName=", 1)[1].split(" State=", 1)[0]
            assert len(name.encode()) <= _MAX_STEP_NAME_LEN, f"{len(name)} bytes: {line[:400]}"
        assert any(ln.rstrip().split(" State=", 1)[0].endswith("...") for ln in step_lines), out
