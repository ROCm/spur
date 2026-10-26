# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Empty batch inputs fail at submission instead of entering scheduling."""

from cluster import parse_job_id


def test_empty_script_file_is_rejected(cluster):
    script = cluster.write_file("empty.sbatch", "", executable=False)
    code, output = cluster.sbatch_with_exit(["--parsable", script])

    assert code != 0
    assert "Batch script is empty!" in output
    assert parse_job_id(output) is None


def test_empty_stdin_is_rejected(cluster):
    command = cluster._cli_command(["sbatch", "--parsable"])
    code, output = cluster.nodes[0].exec_with_exit(f"{command} < /dev/null")

    assert code != 0
    assert "Batch script is empty!" in output
    assert parse_job_id(output) is None
