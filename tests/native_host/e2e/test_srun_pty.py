# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `srun --pty` in step mode (spur#780).

Inside an allocation, `srun --pty <cmd>` must run the command through an
interactive PTY session (the InteractiveSession path) rather than the buffered
RunStep path — the same machinery `srun --jobid --overlap --pty` uses — instead
of silently dropping the flag and running non-interactively.
"""


class TestSrunPtyStep:
    def test_pty_step_runs_and_returns_output(self, cluster):
        code, out = cluster.salloc_run("srun --pty echo PTY-MARKER-BRAVO\n")
        assert code == 0, out
        assert "PTY-MARKER-BRAVO" in out, out
        # --pty is honored now, so the "not yet honored" warning must be gone.
        assert "not yet honored" not in out, out

    def test_pty_step_propagates_exit_code(self, cluster):
        code, out = cluster.salloc_run(
            'srun --pty bash -c "exit 9" || echo "pty-exit=$?"\n'
        )
        assert code == 0, out
        assert "pty-exit=9" in out, out
