# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""RCCL collectives launched through Spur's PMIx plugin.

Spur already covers the two halves separately: `test_mpi.py` proves `srun --mpi=pmix`
hands out a correct rank map, and `fixtures/distributed_test.py` drives RCCL through
PyTorch on a single node. Neither covers an MPI-launched RCCL communicator, and
`distributed_test.py` states outright that its multi-node mode does no cross-node
RCCL. That combination is what every real MPI-based collective workload uses, so it
is what these tests add.

Two dimensions, both needed:

* intra-node, several ranks sharing one host and one GPU each, which exercises the
  local rank map and device assignment;
* inter-node, ranks split across hosts, which is the only case that puts RCCL
  traffic on the wire and therefore the only one that can catch a transport or
  interface problem.

All tests skip cleanly without GPUs, ROCm devel packages, RCCL, or the PMIx plugin,
so they are inert on a CPU-only runner rather than failing it.
"""

import re

import pytest


# A wrong result means the collective ran but moved the wrong data, which is a
# different failure from a hang. Parsed separately from the rank map for that reason.
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
    """Every rank must report OK, and the sum must be the one only a real
    all-reduce produces."""
    results = ALLREDUCE_RE.findall(out)
    assert len(results) == expected_size, (
        f"expected {expected_size} allreduce lines, got {len(results)}:\n{out}"
    )
    # Each rank contributes (rank + 1), so a correct sum is n(n+1)/2. Asserting the
    # value and not just the status keeps the test honest if the fixture's own
    # comparison is ever weakened.
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
        """The smallest MPI-launched RCCL collective: two ranks, one host.

        Needs two visible GPUs. With one GPU both ranks would share a device, which
        RCCL rejects rather than silently serialising, so the test skips instead of
        reporting a failure that is really a hardware shortfall.
        """
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

        Two ranks on the same device would still produce the right sum, so the
        arithmetic assertion alone cannot catch it. Compared on PCI bus id rather
        than device ordinal: Spur narrows each task's visible set to the GPU it was
        granted and ROCr renumbers what is left, so both ranks report ordinal 0 even
        when they are correctly on different hardware.
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
        """The case that matters most: RCCL traffic actually crosses the wire.

        This is the only test here that can catch a transport problem, since every
        intra-node collective stays inside one host and never touches the fabric.
        """
        cluster = mpi_multi_node_cluster
        cluster.rccl_preflight(2)
        binary = cluster.compile_rccl_fixture()

        code, out = cluster.srun_with_exit(["--mpi=pmix", "-N", "2", "-n", "2", "--gres=gpu:1", binary])
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        ranks, size, hosts = parse_ranks(out)
        assert ranks == {0, 1}, f"expected ranks 0-1, got {ranks}:\n{out}"
        assert size == 2, f"expected size=2, got {size}:\n{out}"
        # The point of the test. Without this, a scheduler that quietly placed both
        # ranks on one node would pass while proving nothing about the fabric.
        assert len(set(hosts.values())) == 2, (
            f"expected 2 distinct hosts, got {hosts}:\n{out}"
        )
        assert_allreduce_correct(out, 2)

    def test_rccl_all_reduce_two_nodes_two_ranks_each(self, mpi_multi_node_cluster):
        """Four ranks over two hosts: intra-node and inter-node paths in one
        communicator, which is the shape real training jobs use."""
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
        # Two ranks per host, so an uneven split means the rank map is wrong even
        # though the collective would still complete.
        per_host: dict[str, int] = {}
        for host in hosts.values():
            per_host[host] = per_host.get(host, 0) + 1
        assert sorted(per_host.values()) == [2, 2], (
            f"expected 2 ranks per host, got {per_host}:\n{out}"
        )
        assert_allreduce_correct(out, 4)

    def test_rccl_survives_back_to_back_launches(self, mpi_multi_node_cluster):
        """A second launch must succeed immediately after the first.

        A communicator or PMIx namespace left behind by the first job shows up here
        and nowhere else, and it is a realistic CI pattern: several collective jobs
        in a row on the same allocation.
        """
        cluster = mpi_multi_node_cluster
        cluster.rccl_preflight(2)
        binary = cluster.compile_rccl_fixture()

        for attempt in range(2):
            code, out = cluster.srun_with_exit(
                ["--mpi=pmix", "-N", "2", "-n", "2", "--gres=gpu:1", binary]
            )
            assert code == 0, f"attempt {attempt} failed (exit {code}):\n{out}"
            assert_allreduce_correct(out, 2)
