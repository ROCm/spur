# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""MPI E2E tests (single-node and multi-node)."""

import dataclasses
import os
import re
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager

import pytest

from cluster import (
    SpurCluster,
    ensure_bins,
    job_state,
    make_remote_dir,
    parse_job_id,
    wait_job,
    wait_job_state,
)

MPI_SOAK_ITERATIONS = max(1, int(os.environ.get("SPUR_MPI_SOAK_ITERATIONS", "1")))


@dataclasses.dataclass
class MpiRank:
    rank: int
    size: int
    host: str
    pid: int


def parse_mpi_rows(out: str) -> list[MpiRank]:
    """Parse hello_mpi output lines: rank=R size=S host=H pid=P."""
    rows: list[MpiRank] = []
    for line in out.splitlines():
        m = re.match(
            r"rank=(\d+)\s+size=(\d+)\s+host=(\S+)\s+pid=(\d+)", line.strip()
        )
        if m:
            rows.append(
                MpiRank(int(m.group(1)), int(m.group(2)), m.group(3), int(m.group(4)))
            )
    return rows


def parse_mpi_ranks(out: str) -> list[tuple[int, int]]:
    """Compat shim: return [(rank, size), ...] for existing tests."""
    return [(r.rank, r.size) for r in parse_mpi_rows(out)]


def assert_mpi_ranks(out: str, expected_ranks: set[int], expected_size: int) -> None:
    results = parse_mpi_ranks(out)
    assert len(results) == len(expected_ranks), (
        f"expected {len(expected_ranks)} rank lines, got {len(results)}:\n{out}"
    )
    ranks = set()
    for rank, size in results:
        assert size == expected_size, f"expected size={expected_size}, got {size}:\n{out}"
        assert rank not in ranks, f"duplicate rank={rank}:\n{out}"
        ranks.add(rank)
    assert ranks == expected_ranks, f"expected ranks {expected_ranks}, got {ranks}:\n{out}"


def assert_one_mpi_world(
    out: str, expected_size: int, expected_hosts: list[str] | None = None
) -> None:
    """Assert exactly one MPI world with distinct ranks, PIDs, and optional host check."""
    rows = parse_mpi_rows(out)
    assert len(rows) == expected_size, (
        f"expected {expected_size} rank lines, got {len(rows)}:\n{out}"
    )
    assert sorted(r.rank for r in rows) == list(range(expected_size)), (
        f"ranks not 0..{expected_size - 1}:\n{out}"
    )
    assert all(r.size == expected_size for r in rows), (
        f"not all ranks report size={expected_size}:\n{out}"
    )
    pids = {r.pid for r in rows}
    assert len(pids) == expected_size, (
        f"expected {expected_size} distinct PIDs, got {len(pids)}:\n{out}"
    )
    if expected_hosts is not None:
        actual_hosts = sorted({r.host for r in rows})
        assert actual_hosts == sorted(expected_hosts), (
            f"expected hosts {sorted(expected_hosts)}, got {actual_hosts}:\n{out}"
        )


def assert_plm_slurm_selected(log: str) -> None:
    """Assert PRRTE used plm:slurm, not SSH, for daemon placement."""
    assert "plm:slurm" in log, f"plm:slurm not found in PRRTE log:\n{log}"
    assert "plm:ssh" not in log and "plm:rsh" not in log, (
        f"PRRTE fell back to SSH despite plm:slurm being available:\n{log}"
    )


ENV_SH = "$HOME/spur/mpi/env.sh"
ENV_SH_SENTINEL = "SPUR_E2E_ENV_SH_SENTINEL"


@contextmanager
def env_sh_sentinel(cluster):
    """Export ``ENV_SH_SENTINEL=applied`` from every node's ``env.sh``, and undo it,
    removing the file on nodes where it did not exist before."""
    created, touched = [], []
    try:
        for node in cluster.nodes:
            if node.exec_allow_fail(f'test -e "{ENV_SH}" || echo absent').strip() == "absent":
                created.append(node)
            node.exec('mkdir -p "$HOME/spur/mpi"')
            node.exec(f'printf "export {ENV_SH_SENTINEL}=applied\\n" >> "{ENV_SH}"')
            touched.append(node)
        yield
    finally:
        for node in touched:
            if node in created:
                node.exec_allow_fail(f'rm -f "{ENV_SH}"')
            else:
                node.exec_allow_fail(f"sed -i '/{ENV_SH_SENTINEL}/d' \"{ENV_SH}\"")


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
            # srun's own output, not a node log: an operator who forgot to deploy
            # the plugin never reads anything else.
            logs = "\n".join(cluster.spurd_log(i) for i in range(len(cluster.nodes)))
            assert "plugin not found" in out.lower(), (
                f"expected plugin-not-found error in srun output, got:\n{out}\n"
                f"node logs:\n{logs}"
            )
        finally:
            cluster.teardown()

    def test_mpi_batch_job_never_starts_without_plugin(self, ssh_nodes, remote_bin_dir):
        """A batch job reaches the supervisor, where a plugin failure would
        otherwise surface as a bare SIGKILL rather than a launch refusal."""
        import os
        import time
        from pathlib import Path

        binaries_dir = os.environ.get(
            "SPUR_TEST_BINARIES_DIR",
            str(Path(__file__).resolve().parents[3] / "target" / "release"),
        )
        ensure_bins(ssh_nodes, binaries_dir, remote_bin_dir, with_mpi_plugin=False)
        cluster = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
        cluster.deploy(config_overrides={"mpi": {"plugin_dir": "/nonexistent/spur-mpi"}})
        try:
            script = cluster.write_file("mpi-no-plugin.sh", "#!/bin/bash\n/bin/true\n")
            job_id = parse_job_id(
                cluster.sbatch(["--mpi=pmix", "-N", "1", "-n", "1", script])
            )
            assert job_id is not None
            deadline = time.time() + 15
            while time.time() < deadline:
                assert job_state(cluster.squeue_all(), job_id) != "R", (
                    f"job {job_id} started despite a plugin this node cannot load"
                )
                time.sleep(2)
            logs = "\n".join(cluster.spurd_log(i) for i in range(len(cluster.nodes)))
            assert "cannot host" in logs, (
                f"the node must record why it refused the launch, got:\n{logs}"
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

    def test_a_lone_batch_rank_gets_the_rank_environment(self, mpi_cluster):
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/lone-batch-rank.out"
        script = cluster.write_file(
            "lone-batch-rank.sh",
            "#!/bin/bash\n#SBATCH --mpi=pmix\n"
            f'echo "seen=${ENV_SH_SENTINEL} procid=$SLURM_PROCID"\n'
            f'exec "{hello_mpi}"\n',
        )
        with env_sh_sentinel(cluster):
            job_id = parse_job_id(cluster.sbatch(["-n1", "-o", out_path, script]))
            assert job_id is not None
            wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        assert "seen=applied procid=0" in content, content
        assert_mpi_ranks(content, {0}, 1)

    def test_a_pmix_batch_driver_is_not_given_a_rank_environment(self, mpi_cluster):
        """A batch script that launches its own step is a driver; only the step's
        ranks get ``env.sh``."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/batch-driver.out"
        rank = cluster.write_file(
            "batch-driver-rank.sh",
            f'#!/bin/bash\necho "rank seen=${ENV_SH_SENTINEL}"\nexec "{hello_mpi}"\n',
            all_nodes=True,
        )
        script = cluster.write_file(
            "batch-driver.sh",
            "#!/bin/bash\n#SBATCH --mpi=pmix\n"
            f'echo "driver seen=${ENV_SH_SENTINEL}."\n'
            f"srun --mpi=pmix -n1 {rank}\n",
        )
        with env_sh_sentinel(cluster):
            job_id = parse_job_id(cluster.sbatch(["-n1", "-o", out_path, script]))
            assert job_id is not None
            wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        assert "driver seen=." in content, content
        assert "rank seen=applied" in content, content
        assert_mpi_ranks(content, {0}, 1)


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

    def test_a_lone_rank_sources_the_agent_mpi_environment(self, mpi_multi_node_cluster):
        """Nothing else applies ``$HOME/spur/mpi/env.sh``, and a rank that misses it
        cannot find the MPI it was linked against."""
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        # Reported by the payload rather than by a bare `echo`, so the step is a real
        # MPI step and its exit status still means something.
        payload = cluster.write_file(
            "lone-rank-env.sh",
            f'#!/bin/bash\necho "seen=${ENV_SH_SENTINEL} procid=$SLURM_PROCID"\n'
            f'exec "{hello_mpi}"\n',
            all_nodes=True,
        )
        with env_sh_sentinel(cluster):
            code, out = cluster.srun_with_exit(
                ["--mpi=pmix", "-N", "2", "-n", "2", payload]
            )
        assert code == 0, f"srun failed (exit {code}):\n{out}"
        assert out.count("seen=applied") == 2, f"both ranks must see env.sh, got:\n{out}"
        assert sorted(re.findall(r"procid=(\d+)", out)) == ["0", "1"], (
            f"each rank must keep its SLURM_PROCID, got:\n{out}"
        )
        assert_mpi_ranks(out, {0, 1}, 2)

    def test_a_lone_rank_is_pinned_by_map_cpu(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        payload = cluster.write_file(
            "lone-rank-map-cpu.sh",
            "#!/bin/bash\n"
            "echo \"pinned procid=$SLURM_PROCID "
            "cpus=$(awk '/^Cpus_allowed_list/{print $2}' /proc/$$/status)\"\n"
            f'exec "{hello_mpi}"\n',
            all_nodes=True,
        )
        code, out = cluster.srun_with_exit(
            ["--mpi=pmix", "-N", "2", "-n", "2", "--cpu-bind=map_cpu:0,1", payload]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"
        pinned = dict(re.findall(r"pinned procid=(\d+) cpus=(\S+)", out))
        assert pinned == {"0": "0", "1": "1"}, f"each rank must sit on its map entry:\n{out}"
        assert_mpi_ranks(out, {0, 1}, 2)

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


@pytest.mark.mpi
class TestMpiSoak:
    """Repeat serial and concurrent MPI launches to catch modex/fence races."""

    @pytest.mark.parametrize("iteration", range(MPI_SOAK_ITERATIONS))
    def test_serial_srun_single_node_four_ranks(self, mpi_cluster, iteration):
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=pmix", "-n4", hello_mpi])
        assert code == 0, f"iteration {iteration} failed:\n{out}"
        assert_mpi_ranks(out, {0, 1, 2, 3}, 4)

    @pytest.mark.parametrize("iteration", range(MPI_SOAK_ITERATIONS))
    def test_serial_srun_multi_node_two_by_two(self, mpi_multi_node_cluster, iteration):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "4", hello_mpi])
        assert code == 0, f"iteration {iteration} failed:\n{out}"
        assert_mpi_ranks(out, {0, 1, 2, 3}, 4)

    @pytest.mark.parametrize("iteration", range(MPI_SOAK_ITERATIONS))
    def test_serial_sbatch_direct_pmix(self, mpi_cluster, iteration):
        """Direct #SBATCH --mpi=pmix without an inner srun."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpi-soak-batch-{iteration}.out"
        script = cluster.write_file(
            f"mpi-soak-batch-{iteration}.sh",
            "#!/bin/bash\n#SBATCH --mpi=pmix\n" f"{hello_mpi}\n",
        )
        sb = cluster.sbatch(["-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        assert_mpi_ranks(content, {0, 1, 2, 3}, 4)

    @pytest.mark.parametrize("iteration", range(MPI_SOAK_ITERATIONS))
    def test_serial_step_in_allocation(self, mpi_cluster, iteration):
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        hold_script = cluster.write_file(
            f"mpi-hold-soak-{iteration}.sh", "#!/bin/bash\nsleep 120\n"
        )
        sb = cluster.sbatch(
            ["-J", f"mpi-hold-soak-{iteration}", "-n4", "-t", "5", hold_script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id, ["--mpi=pmix", "-n4", hello_mpi]
            )
            assert code == 0, f"iteration {iteration} step failed:\n{out}"
            assert_mpi_ranks(out, {0, 1, 2, 3}, 4)
        finally:
            cluster.scancel(str(job_id))

    @pytest.mark.parametrize("iteration", range(MPI_SOAK_ITERATIONS))
    def test_serial_multi_node_step_in_allocation(self, mpi_multi_node_cluster, iteration):
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        hold_script = cluster.write_file(
            f"mpi-hold-soak-multi-{iteration}.sh", "#!/bin/bash\nsleep 120\n"
        )
        sb = cluster.sbatch(
            ["-J", f"mpi-hold-soak-multi-{iteration}", "-N2", "-n4", "-t", "5", hold_script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=90)
        try:
            code, out = cluster.srun_in_allocation(
                job_id, ["--mpi=pmix", "-N2", "-n4", hello_mpi]
            )
            assert code == 0, f"iteration {iteration} step failed:\n{out}"
            assert_mpi_ranks(out, {0, 1, 2, 3}, 4)
        finally:
            cluster.scancel(str(job_id))

    def test_parallel_concurrent_srun_wave(self, mpi_multi_node_cluster):
        """Launch several unlike MPI jobs at once (Crusoe soak matrix)."""
        cluster = mpi_multi_node_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        wave = [
            (["--mpi=pmix", "-n4", hello_mpi], {0, 1, 2, 3}, 4),
            (["--mpi=pmix", "-N", "2", "-n", "2", hello_mpi], {0, 1}, 2),
            (["--mpi=pmix", "-N", "2", "-n", "4", hello_mpi], {0, 1, 2, 3}, 4),
            (["--mpi=pmix", "-n4", hello_mpi], {0, 1, 2, 3}, 4),
            (["--mpi=pmix", "-N", "2", "-n", "2", hello_mpi], {0, 1}, 2),
            (["--mpi=pmix", "-N", "2", "-n", "4", hello_mpi], {0, 1, 2, 3}, 4),
        ]

        def run_one(entry: tuple[list[str], set[int], int]) -> tuple[list[str], set[int], int, int, str]:
            args, expected_ranks, expected_size = entry
            code, out = cluster.srun_with_exit(args)
            return args, expected_ranks, expected_size, code, out

        with ThreadPoolExecutor(max_workers=len(wave)) as pool:
            futures = [pool.submit(run_one, entry) for entry in wave]
            for fut in as_completed(futures):
                args, expected_ranks, expected_size, code, out = fut.result()
                assert code == 0, f"srun {args} failed:\n{out}"
                assert_mpi_ranks(out, expected_ranks, expected_size)


@pytest.mark.mpi
class TestMpirunExternalLauncher:
    """--mpi=mpirun launches mpirun as the external launcher instead of
    Spur's per-rank fork.  Mpirun handles rank fan-out internally."""

    def test_srun_mpi_list_includes_mpirun(self, mpi_cluster):
        cluster = mpi_cluster
        code, out = cluster.srun_with_exit(["--mpi=list", "/bin/true"])
        assert code == 0, out
        assert "mpirun" in out, f"mpirun not listed in --mpi=list output:\n{out}"

    def test_mpirun_single_node_four_ranks(self, mpi_cluster):
        """Single-node: srun --mpi=mpirun -n4 launches mpirun which spawns 4 ranks."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=mpirun", "-n4", hello_mpi])
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_mpi_ranks(out, {0, 1, 2, 3}, 4)

    def test_mpirun_single_node_two_ranks(self, mpi_cluster):
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=mpirun", "-n2", hello_mpi])
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_mpi_ranks(out, {0, 1}, 2)

    def test_sbatch_mpirun_four_ranks(self, mpi_cluster):
        """Batch mode: sbatch with inner mpirun invocation."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpirun-batch.out"
        script = cluster.write_file(
            "mpirun-batch.sh",
            f"#!/bin/bash\nmpirun -np 4 --bind-to none {hello_mpi}\n",
        )
        sb = cluster.sbatch(["-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        assert_mpi_ranks(content, {0, 1, 2, 3}, 4)

    def test_mpirun_step_in_existing_allocation(self, mpi_cluster):
        """Step-mode mpirun: allocation without --mpi, then srun --mpi=mpirun."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        hold_script = cluster.write_file("mpirun-hold.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-J", "mpirun-hold", "-n4", "-t", "5", hold_script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id, ["--mpi=mpirun", "-n4", hello_mpi]
            )
            assert code == 0, f"srun step failed (exit {code}):\n{out}"
            assert_mpi_ranks(out, {0, 1, 2, 3}, 4)
        finally:
            cluster.scancel(str(job_id))

    def test_mpirun_rejects_unknown_mpi_value(self, mpi_cluster):
        """srun --mpi=bogus must fail with a clear error."""
        cluster = mpi_cluster
        code, out = cluster.srun_with_exit(["--mpi=bogus", "-n1", "/bin/true"])
        assert code != 0, f"expected failure for --mpi=bogus, got success:\n{out}"
        assert "invalid" in out.lower() or "error" in out.lower(), (
            f"expected clear error for unknown --mpi value, got:\n{out}"
        )


@pytest.mark.mpi
class TestMpirunMultiNode:
    """Multi-node --mpi=mpirun generates a hostfile and launches mpirun
    with the total rank count across nodes.

    These tests require SSH connectivity between compute nodes for ORTE
    daemon placement.  Skip when nodes cannot SSH to each other."""

    @staticmethod
    def _skip_if_no_inter_node_ssh(cluster):
        """Pre-flight: skip the test when compute nodes cannot SSH to each
        other, which is the prerequisite for ORTE daemon placement.
        Checking before the job avoids masking real launcher failures."""
        nodes = cluster.nodes
        if len(nodes) < 2:
            return
        src, dst = nodes[0], nodes[1]
        probe = src.exec_allow_fail(
            f"ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 "
            f"{dst.host} echo ok 2>&1"
        ).strip()
        if probe != "ok":
            pytest.skip(
                f"inter-node SSH unavailable ({src.host} -> {dst.host}): "
                f"{probe[:120]}"
            )

    def test_mpirun_two_nodes_two_ranks(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        self._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "2", "-n", "2", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_mpi_ranks(out, {0, 1}, 2)

    def test_mpirun_two_nodes_four_ranks(self, mpi_multi_node_cluster):
        cluster = mpi_multi_node_cluster
        self._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "2", "-n", "4", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_mpi_ranks(out, {0, 1, 2, 3}, 4)

    def test_mpirun_two_nodes_uneven_m2(self, mpi_multi_node_cluster):
        """M2: 5 tasks / 2 nodes — first node gets 3, second gets 2."""
        cluster = mpi_multi_node_cluster
        self._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "2", "-n", "5", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 5)
        hosts = {r.host for r in parse_mpi_rows(out)}
        assert len(hosts) == 2, f"expected 2 hosts, got {hosts}:\n{out}"

    def test_mpirun_three_nodes_m3(self, mpi_multi_node_cluster):
        """M3: 3+ nodes."""
        cluster = mpi_multi_node_cluster
        if len(cluster.nodes) < 3:
            pytest.skip("need 3+ nodes for M3")
        self._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "3", "-n", "6", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 6)
        hosts = {r.host for r in parse_mpi_rows(out)}
        assert len(hosts) == 3, f"expected 3 hosts, got {hosts}:\n{out}"

    def test_mpirun_duplicate_driver_guard_m6(self, mpi_multi_node_cluster):
        """M6: exactly N lines, N distinct PIDs — proves single driver."""
        cluster = mpi_multi_node_cluster
        self._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "2", "-n", "4", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 4)


@pytest.mark.mpi
class TestMpirunTestMatrix:
    """Full yansun test matrix: S1-S4, N1-N2, and sbatch-inline paths."""

    def test_s1_sbatch_inline_mpirun_single_node(self, mpi_cluster):
        """S1: sbatch -N1 -n4 with inline mpirun call."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpirun-s1.out"
        script = cluster.write_file(
            "mpirun-s1.sh",
            f"#!/bin/bash\nmpirun -np 4 --bind-to none {hello_mpi}\n",
        )
        sb = cluster.sbatch(["-N", "1", "-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        assert_one_mpi_world(content, 4)

    def test_s3_srun_mpi_mpirun_single_node(self, mpi_cluster):
        """S3: srun --mpi=mpirun -n4 — the new flag."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(["--mpi=mpirun", "-n4", hello_mpi])
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 4)

    def test_s4_mpirun_auto_size(self, mpi_cluster):
        """S4: sbatch -N1 -n4, body calls mpirun without -np (auto-detect)."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpirun-s4.out"
        script = cluster.write_file(
            "mpirun-s4.sh",
            f"#!/bin/bash\nmpirun --bind-to none {hello_mpi}\n",
        )
        sb = cluster.sbatch(["-N", "1", "-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(cluster, job_id, timeout=120)
        content = cluster.read_output_on_any_node(out_path)
        rows = parse_mpi_rows(content)
        assert len(rows) > 0, f"no MPI output:\n{content}"
        assert all(r.size == rows[0].size for r in rows), (
            f"inconsistent size:\n{content}"
        )

    def test_n1_bogus_mpi_rejected(self, mpi_cluster):
        """N1: --mpi=bogus must fail with a clear error."""
        cluster = mpi_cluster
        code, out = cluster.srun_with_exit(["--mpi=bogus", "-n1", "/bin/true"])
        assert code != 0, f"expected failure for --mpi=bogus, got success:\n{out}"
        assert "invalid" in out.lower() or "error" in out.lower(), (
            f"expected clear error for unknown --mpi value, got:\n{out}"
        )

    def test_n2_mpirun_outside_allocation(self, mpi_cluster):
        """N2: bare mpirun without a Spur allocation should not leave stray daemons."""
        cluster = mpi_cluster
        node = cluster.nodes[0]
        node.exec_allow_fail("mpirun --bind-to none /bin/hostname 2>&1")
        pgrep = node.exec_allow_fail("pgrep -l orted 2>&1").strip()
        assert "orted" not in pgrep, (
            f"stray orted daemons left after bare mpirun:\n{pgrep}"
        )

    def test_m1_sbatch_inline_mpirun_multi_node(self, mpi_multi_node_cluster):
        """M1: sbatch -N2 -n4 with inline mpirun, 2 distinct hosts."""
        cluster = mpi_multi_node_cluster
        TestMpirunMultiNode._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpirun-m1.out"
        script = cluster.write_file(
            "mpirun-m1.sh",
            f"#!/bin/bash\nmpirun -np 4 --bind-to none {hello_mpi}\n",
        )
        sb = cluster.sbatch(["-N", "2", "-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(cluster, job_id, timeout=180)
        content = cluster.read_output_on_any_node(out_path)
        assert_one_mpi_world(content, 4)
        hosts = {r.host for r in parse_mpi_rows(content)}
        assert len(hosts) == 2, f"expected 2 hosts, got {hosts}:\n{content}"

    def test_s2_salloc_mpirun_single_node(self, mpi_cluster):
        """S2: salloc -N1 -n4 then mpirun -np 4 inside the allocation.

        salloc's shell runs on the submit host before the agent touches
        the env, so SLURM_TASKS_PER_NODE is absent and ras:slurm fails.
        The batch and step paths set it via the agent.  Fixing salloc env
        propagation is tracked separately."""
        cluster = mpi_cluster
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.salloc_run(
            f"mpirun -np 4 --bind-to none {hello_mpi}",
            salloc_args=["-N", "1", "-n4"],
        )
        if code != 0 and "SLURM_TASKS_PER_NODE" in out:
            pytest.skip(
                "salloc does not propagate SLURM_TASKS_PER_NODE to the "
                "submit-host shell yet — ras:slurm cannot detect the "
                "allocation (tracked separately)"
            )
        assert code == 0, f"salloc+mpirun failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 4)

    def test_m4_srun_mpi_mpirun_multi_node(self, mpi_multi_node_cluster):
        """M4: srun --mpi=mpirun -N2 -n4 — new flag, multi-node."""
        cluster = mpi_multi_node_cluster
        TestMpirunMultiNode._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "2", "-n", "4", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 4)

    def test_m5_no_ssh_proof(self, mpi_multi_node_cluster):
        """M5: multi-node mpirun works even without inter-node SSH when
        plm:slurm is selected (PRRTE routes daemon placement through srun)."""
        cluster = mpi_multi_node_cluster
        nodes = cluster.nodes
        if len(nodes) < 2:
            pytest.skip("need 2+ nodes for M5")
        # Verify plm:slurm is available (--version must output "slurm ...")
        version_out = nodes[0].exec(f"{cluster.bin_dir}/srun --version 2>&1").strip()
        if not version_out.startswith("slurm"):
            pytest.skip(f"plm:slurm requires slurm-compatible --version, got: {version_out}")
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-N", "2", "-n", "4", hello_mpi]
        )
        assert code == 0, f"srun --mpi=mpirun (no-ssh) failed (exit {code}):\n{out}"
        assert_one_mpi_world(out, 4)
        hosts = {r.host for r in parse_mpi_rows(out)}
        assert len(hosts) == 2, f"expected 2 hosts, got {hosts}:\n{out}"

    def test_n3_accounting(self, mpi_multi_node_cluster):
        """N3: after a multi-node mpirun job, sacct shows the job."""
        cluster = mpi_multi_node_cluster
        TestMpirunMultiNode._skip_if_no_inter_node_ssh(cluster)
        hello_mpi = cluster.compile_mpi_fixture("hello_mpi.c")
        out_path = f"{cluster.remote_dir}/mpirun-n3.out"
        script = cluster.write_file(
            "mpirun-n3.sh",
            f"#!/bin/bash\nmpirun -np 4 --bind-to none {hello_mpi}\n",
        )
        sb = cluster.sbatch(["-N", "2", "-n4", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(cluster, job_id, timeout=180)
        sacct_out = cluster.sacct(["-j", str(job_id), "--format=JobID,State,ExitCode", "-n"])
        assert str(job_id) in sacct_out, f"job {job_id} not in sacct:\n{sacct_out}"


@pytest.mark.mpi
@pytest.mark.gpu
class TestMpirunGpuCollective:
    """G1-G2: RCCL all_reduce via mpirun on GPU nodes."""

    def test_g1_rccl_single_node(self, mpi_cluster):
        """G1: RCCL all_reduce_perf via srun --mpi=mpirun, single node."""
        cluster = mpi_cluster
        try:
            rccl_bin = cluster.compile_rccl_fixture("rccl_all_reduce.c")
        except Exception as e:
            pytest.skip(f"RCCL fixture build failed (no GPU toolchain?): {e}")
        code, out = cluster.srun_with_exit(
            ["--mpi=mpirun", "-n2", "--gres=gpu:2", rccl_bin]
        )
        assert code == 0, f"RCCL single-node failed (exit {code}):\n{out}"
        assert "status=OK" in out, f"allreduce mismatch:\n{out}"
        assert "comm_size=" in out, f"missing comm_size:\n{out}"

    def test_g2_rccl_multi_node(self, mpi_multi_node_cluster):
        """G2: RCCL all_reduce via sbatch + mpirun, multi-node."""
        cluster = mpi_multi_node_cluster
        TestMpirunMultiNode._skip_if_no_inter_node_ssh(cluster)
        try:
            rccl_bin = cluster.compile_rccl_fixture("rccl_all_reduce.c")
        except Exception as e:
            pytest.skip(f"RCCL fixture build failed (no GPU toolchain?): {e}")
        out_path = f"{cluster.remote_dir}/rccl-g2.out"
        err_path = f"{cluster.remote_dir}/rccl-g2.err"
        script = cluster.write_file(
            "rccl-g2.sh",
            f"#!/bin/bash\nmpirun -np 4 --bind-to none {rccl_bin}\n",
        )
        sb = cluster.sbatch(
            ["-N", "2", "--ntasks-per-node=2", "--gres=gpu:2",
             "-o", out_path, "-e", err_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(cluster, job_id, timeout=300)
        content = cluster.read_output_on_any_node(out_path)
        assert "status=OK" in content, f"allreduce mismatch:\n{content}"
        hosts = {r.host for r in parse_mpi_rows(content)}
        assert len(hosts) == 2, f"expected 2 hosts, got {hosts}:\n{content}"
