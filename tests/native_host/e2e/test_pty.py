# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for interactive sessions: srun --pty, sattach, and --overlap.

The CLI tolerates a non-TTY stdin (it warns and skips raw mode), so these run
over the ordinary SSH channel. PTY output carries CR line endings, so
assertions match substrings rather than whole lines.
"""

from cluster import parse_job_id, wait_job_state
import pytest

pytestmark = pytest.mark.suite_api


def _clean(output: str) -> str:
    return output.replace("\r", "")


def _start_long_job(cluster, name: str, body: str = "sleep 300") -> int:
    script = cluster.write_file(f"{name}.sh", f"#!/bin/bash\n{body}\n")
    job_id = parse_job_id(cluster.sbatch(["-J", name, script]))
    assert job_id is not None
    wait_job_state(cluster, job_id, "R", timeout=60)
    return job_id


class TestSrunPty:
    def test_pty_runs_command_and_returns_output(self, cluster):
        code, out = cluster.srun_with_exit(
            ["--pty", "bash", "-c", "echo PTY_MARKER"]
        )
        assert code == 0, f"srun --pty failed (exit {code}):\n{out}"
        assert "PTY_MARKER" in _clean(out), (
            f"srun --pty must return the command's output:\n{out}"
        )

    def test_pty_propagates_exit_code(self, cluster):
        code, out = cluster.srun_with_exit(["--pty", "bash", "-c", "exit 7"])
        assert code == 7, f"srun --pty must propagate exit 7, got {code}:\n{out}"

    def test_pty_attaches_to_requested_node(self, multi_node_cluster):
        """--pty must open the session on the -w node, not the first allocated."""
        cluster = multi_node_cluster
        target = cluster.node_names[1]

        code, out = cluster.srun_with_exit(
            ["--pty", "-N", "1", "-w", target, "bash", "-c", "echo host=$(hostname -s)"]
        )
        assert code == 0, f"srun --pty -w {target} failed (exit {code}):\n{out}"

        cleaned = _clean(out)
        assert f"host={target}" in cleaned, (
            f"--pty session must land on {target}, got:\n{cleaned}"
        )


class TestSattach:
    def test_output_only_streams_running_job(self, cluster):
        cluster.agent_resolution_preflight()
        job_id = _start_long_job(
            cluster,
            "sattach-stream",
            "for i in $(seq 1 15); do echo SATTACH_MARKER; sleep 1; done",
        )
        try:
            code, out = cluster.sattach(str(job_id), ["--output-only"])
            assert "SATTACH_MARKER" in _clean(out), (
                f"sattach --output-only must stream the job's stdout (exit {code}):\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_attach_rejects_job_that_is_not_running(self, cluster):
        script = cluster.write_file("sattach-held.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "sattach-held", "-H", script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "PD", timeout=30)
        try:
            code, out = cluster.sattach(str(job_id))
            assert code != 0, f"sattach to a held job must fail, got:\n{out}"
            assert "not running" in out.lower(), (
                f"expected a not-running message, got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_attach_rejects_malformed_job_step(self, cluster):
        code, out = cluster.sattach("not-a-job")
        assert code != 0, f"a malformed job step must be rejected, got:\n{out}"
        assert "invalid job id" in out.lower(), (
            f"expected an invalid-job-ID message, got:\n{out}"
        )


class TestOverlapAttach:
    def test_overlap_execs_inside_running_job(self, cluster):
        job_id = _start_long_job(cluster, "overlap-job")
        try:
            code, out = cluster.srun_with_exit(
                [
                    "--jobid", str(job_id),
                    "--overlap",
                    "bash", "-c", "echo OVERLAP_MARKER",
                ]
            )
            assert code == 0, f"--overlap exec failed (exit {code}):\n{out}"
            assert "OVERLAP_MARKER" in _clean(out), (
                f"--overlap must run the command inside job {job_id}:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_overlap_lands_on_job_node(self, multi_node_cluster):
        """The overlap session must join the job's node, not an arbitrary one."""
        cluster = multi_node_cluster
        target = cluster.node_names[1]
        script = cluster.write_file("overlap-node.sh", "#!/bin/bash\nsleep 300\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "overlap-node", "-N", "1", "-w", target, script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=60)

        try:
            code, out = cluster.srun_with_exit(
                [
                    "--jobid", str(job_id),
                    "--overlap",
                    "bash", "-c", "echo host=$(hostname -s)",
                ]
            )
            assert code == 0, f"--overlap exec failed (exit {code}):\n{out}"
            assert f"host={target}" in _clean(out), (
                f"overlap session must run on {target}, got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_jobid_without_overlap_is_rejected(self, cluster):
        code, out = cluster.srun_with_exit(["--jobid", "1", "hostname"])
        assert code != 0, f"--jobid without --overlap must fail, got:\n{out}"
        assert "--overlap" in out, f"expected an --overlap hint, got:\n{out}"
