# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Single-node MPI E2E tests."""

import re

import pytest

from cluster import SpurCluster, ensure_bins, make_remote_dir, parse_job_id, wait_job, wait_job_state


@pytest.mark.mpi
class TestMpiSingleNode:
    def test_spurd_starts_without_libpmix_on_path(self, mpi_cluster):
        cluster = mpi_cluster
        for node in cluster.nodes:
            ldd = node.exec(f"ldd '{cluster.bin_dir}/spurd'")
            assert "libpmix" not in ldd.lower(), f"spurd must not link libpmix:\n{ldd}"

    def test_srun_mpi_list(self, mpi_cluster):
        cluster = mpi_cluster
        code, out = cluster.srun_with_exit(["--mpi=list", "/bin/true"])
        assert code == 0, out
        assert "none" in out
        assert "pmix" in out

    def test_mpi_job_fails_without_plugin(self, ssh_nodes, remote_bin_dir):
        import os
        from pathlib import Path

        binaries_dir = os.environ.get(
            "SPUR_TEST_BINARIES_DIR",
            str(Path(__file__).resolve().parents[3] / "target" / "release"),
        )
        ensure_bins(ssh_nodes, binaries_dir, remote_bin_dir, with_mpi_plugin=False)
        cluster = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
        cluster.deploy(
            config_overrides={
                "mpi": {
                    "plugin_dir": "/nonexistent/spur-mpi",
                }
            }
        )
        try:
            code, out = cluster.srun_with_exit(["--mpi=pmix", "-n1", "/bin/true"])
            assert code != 0, f"expected failure without plugin, got success:\n{out}"
            combined = f"{out}\n{cluster.spurd_log(0)}"
            assert "MPI plugin not found" in combined or "plugin not found" in combined.lower(), (
                f"expected plugin-not-found error, got:\n{combined}"
            )
        finally:
            cluster.teardown()

    def test_hello_mpi_single_node_four_ranks(self, mpi_cluster):
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=pmix", "-n4", hello_mpi])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks = set()
        for line in out.splitlines():
            match = re.match(r"rank=(\d+) size=(\d+)", line.strip())
            if match:
                ranks.add(int(match.group(1)))
                assert int(match.group(2)) == 4
        assert ranks == {0, 1, 2, 3}, f"expected ranks 0-3, got {ranks}:\n{out}"

    def test_srun_mpi_pmix_in_existing_allocation(self, mpi_cluster):
        """Step-mode PMIx: allocation without --mpi, then srun --mpi=pmix (salloc-like)."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        hold_script = cluster.write_file("mpi-hold.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-J", "mpi-hold", "-n4", "-t", "5", hold_script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id, ["--mpi=pmix", "-n4", hello_mpi]
            )
            assert code == 0, f"srun step failed (exit {code}):\n{out}"

            ranks = set()
            for line in out.splitlines():
                match = re.match(r"rank=(\d+) size=(\d+)", line.strip())
                if match:
                    ranks.add(int(match.group(1)))
                    assert int(match.group(2)) == 4
            assert ranks == {0, 1, 2, 3}, f"expected ranks 0-3, got {ranks}:\n{out}"
        finally:
            cluster.scancel(str(job_id))

    def test_sbatch_mpi_pmix_four_ranks(self, mpi_cluster):
        """Batch launch with #SBATCH --mpi=pmix."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpi-batch.out"
        script = cluster.write_file(
            "mpi-batch.sh",
            "#!/bin/bash\n#SBATCH --mpi=pmix\n" f"{hello_mpi}\n",
        )
        sb = cluster.sbatch(["-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)

        ranks = set()
        for line in content.splitlines():
            match = re.match(r"rank=(\d+) size=(\d+)", line.strip())
            if match:
                ranks.add(int(match.group(1)))
                assert int(match.group(2)) == 4
        assert ranks == {0, 1, 2, 3}, f"expected ranks 0-3, got {ranks}:\n{content}"
