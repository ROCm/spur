# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""RCCL collectives launched through Spur's PMIx plugin.

Inter-node cases are the ones that put RCCL traffic on the wire; the intra-node ones
cover the local rank map and device assignment. Skips without GPUs, RCCL or the plugin.
"""

import re

import pytest


RANK_RE = re.compile(r"rank=(\d+) size=(\d+) host=(\S+) device=(\d+) bus=(\S+)")
ALLREDUCE_RE = re.compile(
    r"allreduce rank=(\d+) expected=(\S+) actual=(\S+) status=(OK|MISMATCH)"
)


def parse_ranks(out: str) -> tuple[set[int], int | None, dict[int, str]]:
    """Return (ranks, size, rank -> host) from the fixture's rank lines."""
    ranks: set[int] = set()
    size: int | None = None
    hosts: dict[int, str] = {}
    for line in out.splitlines():
        m = RANK_RE.match(line.strip())
        if m:
            rank = int(m.group(1))
            ranks.add(rank)
            size = int(m.group(2))
            hosts[rank] = m.group(3)
    return ranks, size, hosts


def assert_allreduce_correct(out: str, expected_size: int) -> None:
    """Every rank reports OK, and the sum is the one only a real all-reduce gives."""
    results = ALLREDUCE_RE.findall(out)
    assert len(results) == expected_size, (
        f"expected {expected_size} allreduce lines, got {len(results)}:\n{out}"
    )
    # Each rank contributes (rank + 1), so a correct sum is n(n+1)/2.
    want = float(expected_size * (expected_size + 1) // 2)
    for rank, expected, actual, status in results:
        assert status == "OK", f"rank {rank} mismatched: {actual} != {expected}\n{out}"
        assert float(expected) == want, (
            f"rank {rank} expected {expected}, arithmetic says {want}:\n{out}"
        )
        assert float(actual) == want, f"rank {rank} got {actual}, want {want}:\n{out}"

    assert f"comm_size={expected_size}" in out, (
        f"rank 0 did not report comm_size={expected_size}:\n{out}"
    )


@pytest.mark.gpu
class TestRcclIntraNode:
    def test_rccl_all_reduce_two_ranks_one_node(self, mpi_cluster):
        """The smallest MPI-launched RCCL collective: two ranks, one host."""
        cluster = mpi_cluster
        binary = cluster.compile_rccl_fixture()
        gpus = cluster.node_gpu_count(cluster.node_names[0])
        if gpus < 2:
            pytest.skip(f"intra-node RCCL needs 2 visible GPUs, found {gpus}")

        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "1", "-n", "2", "--gres=gpu:2", binary])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks, size, hosts = parse_ranks(out)
        assert ranks == {0, 1}, f"expected ranks 0-1, got {ranks}:\n{out}"
        assert size == 2, f"expected size=2, got {size}:\n{out}"
        assert len(set(hosts.values())) == 1, (
            f"intra-node run spanned hosts {set(hosts.values())}:\n{out}"
        )
        assert_allreduce_correct(out, 2)

    def test_each_rank_gets_a_distinct_device(self, mpi_cluster):
        """Ranks on one host must land on different physical GPUs.

        Compared on PCI bus id, not ordinal: each task's visible set is narrowed to
        its own GPU and renumbered, so every rank correctly reports device 0.
        """
        cluster = mpi_cluster
        binary = cluster.compile_rccl_fixture()
        gpus = cluster.node_gpu_count(cluster.node_names[0])
        if gpus < 2:
            pytest.skip(f"device-distinctness needs 2 visible GPUs, found {gpus}")

        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "1", "-n", "2", "--gres=gpu:2", binary])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        buses = [m.group(5) for m in map(RANK_RE.match, map(str.strip, out.splitlines())) if m]
        assert len(buses) == 2, f"expected 2 rank lines, got {buses}:\n{out}"
        assert len(set(buses)) == 2, f"ranks shared a GPU: {buses}\n{out}"


@pytest.mark.gpu
class TestRcclInterNode:
    def test_rccl_all_reduce_two_nodes_one_rank_each(self, mpi_multi_node_cluster):
        """The only shape that puts RCCL traffic on the fabric."""
        cluster = mpi_multi_node_cluster
        cluster.rccl_preflight(2)
        binary = cluster.compile_rccl_fixture()

        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "2", "--gres=gpu:1", binary])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks, size, hosts = parse_ranks(out)
        assert ranks == {0, 1}, f"expected ranks 0-1, got {ranks}:\n{out}"
        assert size == 2, f"expected size=2, got {size}:\n{out}"
        # Both ranks on one node would pass every other assertion here.
        assert len(set(hosts.values())) == 2, (
            f"expected 2 distinct hosts, got {hosts}:\n{out}"
        )
        assert_allreduce_correct(out, 2)

    def test_rccl_all_reduce_two_nodes_two_ranks_each(self, mpi_multi_node_cluster):
        """Four ranks over two hosts: both paths in one communicator."""
        cluster = mpi_multi_node_cluster
        cluster.rccl_preflight(2)
        binary = cluster.compile_rccl_fixture()
        gpus = cluster.node_gpu_count(cluster.node_names[0])
        if gpus < 2:
            pytest.skip(f"2 ranks per node needs 2 visible GPUs, found {gpus}")

        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "4", "--gres=gpu:2", binary])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks, size, hosts = parse_ranks(out)
        assert ranks == {0, 1, 2, 3}, f"expected ranks 0-3, got {ranks}:\n{out}"
        assert size == 4, f"expected size=4, got {size}:\n{out}"
        assert len(set(hosts.values())) == 2, (
            f"expected 4 ranks over 2 hosts, got {hosts}:\n{out}"
        )
        # An uneven split means a wrong rank map; the collective still completes.
        per_host: dict[str, int] = {}
        for host in hosts.values():
            per_host[host] = per_host.get(host, 0) + 1
        assert sorted(per_host.values()) == [2, 2], (
            f"expected 2 ranks per host, got {per_host}:\n{out}"
        )
        assert_allreduce_correct(out, 4)

    def test_rccl_survives_back_to_back_launches(self, mpi_multi_node_cluster):
        """A namespace left behind by the first job shows up here and nowhere else."""
        cluster = mpi_multi_node_cluster
        cluster.rccl_preflight(2)
        binary = cluster.compile_rccl_fixture()

        for attempt in range(2):
            code, out = cluster.srun_with_exit(
                ["--mpi=pmix", "-N", "2", "-n", "2", "--gres=gpu:1", binary]
            )
            assert code == 0, f"attempt {attempt} failed (exit {code}):\n{out}"
            assert_allreduce_correct(out, 2)
