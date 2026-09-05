# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `srun --pty` on a job step.

Inside an allocation, `srun --pty <cmd>` must run the command through an
interactive PTY session (the InteractiveSession path) rather than the buffered
RunStep path — the same machinery `srun --jobid --overlap --pty` uses — instead
of silently dropping the flag and running non-interactively.
"""


class TestSrunPtyStep:
    def test_pty_step_allocates_a_real_tty(self, cluster):
        # The agent allocates a real PTY server-side only on the interactive
        # path, so stdout is a TTY inside the command. On the old buffered path
        # `test -t 1` is false, so this fails if the --pty routing regresses.
        code, out = cluster.salloc_run(
            "srun --pty bash -c 'test -t 1 && echo IS-TTY || echo NO-TTY'\n"
        )
        assert code == 0, out
        assert "IS-TTY" in out, out
        assert "NO-TTY" not in out, out

    def test_pty_step_runs_and_returns_output(self, cluster):
        code, out = cluster.salloc_run("srun --pty echo PTY-MARKER-BRAVO\n")
        assert code == 0, out
        assert "PTY-MARKER-BRAVO" in out, out

    def test_pty_step_propagates_exit_code(self, cluster):
        code, out = cluster.salloc_run(
            'srun --pty bash -c "exit 9" || echo "pty-exit=$?"\n'
        )
        assert code == 0, out
        assert "pty-exit=9" in out, out
