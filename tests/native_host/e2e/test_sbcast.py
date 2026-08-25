# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `sbcast` — broadcasting a file to a job's allocated nodes."""

from cluster import parse_job_id, wait_job_state


class TestSbcast:
    def test_sbcast_delivers_file_to_running_job_node(self, cluster):
        # A job that stays running long enough to sbcast into.
        script = cluster.write_file(
            "sbcast-sleeper.sh", "#!/bin/bash\nsleep 300\n"
        )
        sb = cluster.sbatch(["-J", "sbcast-job", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=60)

        try:
            payload = "SBCAST_PAYLOAD_OK\nline2\n"
            src = cluster.write_file("sbcast-src.dat", payload, executable=False)
            dest = f"{cluster.remote_dir}/sbcast-dst.dat"
            # Ensure a clean slate for the destination.
            cluster.nodes[0].exec_allow_fail(f"rm -f '{dest}'")

            out = cluster.cli(["sbcast", "--jobid", str(job_id), src, dest])
            assert "node(s)" in out, f"unexpected sbcast output:\n{out}"

            got = cluster.read_output_on_any_node(dest)
            assert "SBCAST_PAYLOAD_OK" in got, f"file not delivered:\n{got}"

            # Without --force, re-sending to an existing path must fail.
            rc, out2 = cluster.cli_with_exit(
                ["sbcast", "--jobid", str(job_id), src, dest]
            )
            assert rc != 0, f"expected failure without --force, got:\n{out2}"

            # With --force, it overwrites.
            payload2 = "SBCAST_FORCED_OK\n"
            src2 = cluster.write_file("sbcast-src2.dat", payload2, executable=False)
            out3 = cluster.cli(
                ["sbcast", "--force", "--jobid", str(job_id), src2, dest]
            )
            assert "node(s)" in out3, f"forced sbcast failed:\n{out3}"
            got2 = cluster.read_output_on_any_node(dest)
            assert "SBCAST_FORCED_OK" in got2, f"force did not overwrite:\n{got2}"
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_sbcast_rejects_nonexistent_job(self, cluster):
        src = cluster.write_file("sbcast-nojob.dat", "x\n", executable=False)
        dest = f"{cluster.remote_dir}/sbcast-nojob-dst.dat"
        rc, out = cluster.cli_with_exit(
            ["sbcast", "--jobid", "999999", src, dest]
        )
        assert rc != 0, f"expected failure for unknown job, got:\n{out}"
