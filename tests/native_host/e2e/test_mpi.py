# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""MPI E2E tests (single-node and multi-node)."""

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


@pytest.mark.mpi
class TestMpiMultiNode:
    def test_hello_mpi_two_nodes_one_rank_each(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "2", hello_mpi])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks = set()
        for line in out.splitlines():
            match = re.match(r"rank=(\d+) size=(\d+)", line.strip())
            if match:
                ranks.add(int(match.group(1)))
                assert int(match.group(2)) == 2
        assert ranks == {0, 1}, f"expected ranks 0-1, got {ranks}:\n{out}"

    def test_hello_mpi_two_nodes_multi_rank(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "4", hello_mpi])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks = set()
        for line in out.splitlines():
            match = re.match(r"rank=(\d+) size=(\d+)", line.strip())
            if match:
                ranks.add(int(match.group(1)))
                assert int(match.group(2)) == 4
        assert ranks == {0, 1, 2, 3}, f"expected ranks 0-3, got {ranks}:\n{out}"

    def test_standalone_srun_pmix(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "2", hello_mpi])
        assert code == 0, f"srun failed (exit {code}):\n{out}"
        assert "rank=" in out

    def test_batch_script_srun_pmix(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpi-batch-multi.out"
        script = cluster.write_file(
            "mpi-batch-multi.sh",
            "#!/bin/bash\n#SBATCH --mpi=pmix\n#SBATCH -N2\n" f"srun --mpi=pmix {hello_mpi}\n",
        )
        sb = cluster.sbatch(["-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job(cluster, job_id, timeout=180)
        content = cluster.read_output_on_any_node(out_path)

        ranks = set()
        for line in content.splitlines():
            match = re.match(r"rank=(\d+) size=(\d+)", line.strip())
            if match:
                ranks.add(int(match.group(1)))
                assert int(match.group(2)) == 4
        assert ranks == {0, 1, 2, 3}, f"expected ranks 0-3, got {ranks}:\n{content}"

    def test_mpi_none_unchanged(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, _out = cluster.srun_with_exit(["-N", "2", "-n", "2", hello_mpi])
        assert code != 0, "MPI_Init should fail without --mpi=pmix"

    def test_sbatch_srun_step_pmix(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        hold_script = cluster.write_file("mpi-hold-multi.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-J", "mpi-hold-multi", "-N2", "-n4", "-t", "5", hold_script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=90)
        try:
            code, out = cluster.srun_in_allocation(
                job_id, ["--mpi=pmix", "-N2", "-n4", hello_mpi]
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
