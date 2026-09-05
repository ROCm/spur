# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for srun step output delivery.

A command run as an srun step inside an allocation has its stdout/stderr
redirected to per-step spool files on the node; the client tails those files
live via StreamJobOutput while the RunStep dispatch is in flight. These tests
drive real steps through an allocation shell and assert their output reaches
the client intact.
"""


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
