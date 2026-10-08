# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for sbatch --wait.

Exercises the full --wait path through a real controller and agent:
exit code propagation, terminal state handling, timeout, script file
with directive, and the no-wait baseline.
"""


class TestSbatchWait:
    def test_wait_success_exit_zero(self, cluster):
        code, _ = cluster.sbatch_with_exit(
            ["--wait", "--parsable", "--wrap", "true", "--export", "NONE"]
        )
        assert code == 0

    def test_wait_propagates_nonzero_exit(self, cluster):
        code, _ = cluster.sbatch_with_exit(
            ["--wait", "--parsable", "--wrap", "exit 42", "--export", "NONE"]
        )
        assert code == 42

    def test_wait_exit_one_false(self, cluster):
        code, _ = cluster.sbatch_with_exit(
            ["--wait", "--parsable", "--wrap", "false", "--export", "NONE"]
        )
        assert code == 1

    def test_wait_exit_255_boundary(self, cluster):
        code, _ = cluster.sbatch_with_exit(
            ["--wait", "--parsable", "--wrap", "exit 255", "--export", "NONE"]
        )
        assert code == 255

    def test_wait_short_flag(self, cluster):
        code, _ = cluster.sbatch_with_exit(
            ["-W", "--parsable", "--wrap", "true", "--export", "NONE"]
        )
        assert code == 0

    def test_wait_with_parsable_prints_id_then_blocks(self, cluster):
        code, out = cluster.sbatch_with_exit(
            ["--wait", "--parsable", "--wrap", "true", "--export", "NONE"]
        )
        assert code == 0
        job_id = out.strip().split("\n")[0]
        assert job_id.isdigit()

    def test_wait_timeout_exits_one(self, cluster):
        code, _ = cluster.sbatch_with_exit(
            [
                "--wait", "--parsable", "--wrap", "sleep 3600",
                "--time=0:00:10", "--export", "NONE",
            ]
        )
        assert code == 1

    def test_wait_script_file_with_directive(self, cluster):
        script = cluster.write_file(
            "wait-directive.sh",
            "#!/bin/bash\n"
            "#SBATCH --wait\n"
            "#SBATCH --parsable\n"
            "#SBATCH --export=NONE\n"
            "exit 3\n",
        )
        code, _ = cluster.sbatch_with_exit([script])
        assert code == 3

    def test_without_wait_returns_immediately(self, cluster):
        code, out = cluster.sbatch_with_exit(
            ["--parsable", "--wrap", "sleep 60", "--export", "NONE"]
        )
        assert code == 0
        job_id = out.strip().split("\n")[0]
        assert job_id.isdigit()
        cluster.cli(["scancel", job_id])
