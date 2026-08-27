# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests that verify exclusive jobs fence their node against co-scheduling.

The specific scenario covered: an exclusive job that requests only 1 CPU (and
optionally 1 GPU) out of a node's full capacity must prevent the backfill
scheduler from placing any other job on that node while the exclusive job runs,
regardless of how many resources remain nominally free.
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


class TestExclusiveBlocking:
    def test_exclusive_cpu_blocks_coscheduling(self, cluster):
        """An exclusive job requesting 1 CPU blocks a second job from landing on
        the same node, even though 63 of 64 CPUs are nominally free."""
        node = cluster.node_names[0]

        # A long-running exclusive job that requests just 1 CPU.
        holder_script = cluster.write_file(
            "excl-holder.sh", "#!/bin/bash\nsleep 300\n"
        )
        holder_id = parse_job_id(
            cluster.sbatch(
                [
                    "-N", "1",
                    "-w", node,
                    "--exclusive",
                    "-c", "1",
                    "-t", "10",
                    holder_script,
                ]
            )
        )
        assert holder_id is not None, "holder job did not submit"

        try:
            wait_job_state(cluster, holder_id, "R", timeout=60)

            # A second job targeting the same node must stay PENDING.
            # Use -c 1 so the request is trivially satisfiable on resources
            # alone — the only reason it should pend is the exclusive hold.
            blocker_script = cluster.write_file(
                "excl-blocked.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
            )
            blocked_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-N", "1",
                        "-w", node,
                        "-c", "1",
                        "-t", "1",
                        blocker_script,
                    ]
                )
            )
            assert blocked_id is not None, "blocked job did not submit"

            try:
                # Give the scheduler several cycles to (incorrectly) start it.
                time.sleep(15)
                sq = cluster.squeue_all()
                lines = [l for l in sq.splitlines() if str(blocked_id) in l]
                assert lines, f"blocked job {blocked_id} not found in squeue:\n{sq}"
                state = lines[0].split()[4] if len(lines[0].split()) > 4 else ""
                assert state == "PD", (
                    f"job {blocked_id} should be PENDING while exclusive job "
                    f"{holder_id} holds the node, but state={state!r}:\n{sq}"
                )
            finally:
                cluster.scancel(str(blocked_id))
        finally:
            cluster.scancel(str(holder_id))

    def test_exclusive_gpu_blocks_coscheduling(self, gpu_cluster):
        """An exclusive job requesting 1 CPU and 1 GPU blocks a second GPU job
        from landing on the same node, even though most GPUs are nominally free."""
        cluster = gpu_cluster
        cluster.gpu_preflight(1)

        node = cluster.node_names[0]
        total_gpus = cluster.node_gpu_count(node)
        if total_gpus < 2:
            pytest.skip(
                f"need >= 2 GPUs on {node} to prove remaining GPUs are blocked "
                f"(found {total_gpus})"
            )

        # Exclusive holder: 1 CPU, 1 GPU — leaving total_gpus-1 GPUs "free".
        holder_script = cluster.write_file(
            "excl-gpu-holder.sh", "#!/bin/bash\nsleep 300\n"
        )
        holder_id = parse_job_id(
            cluster.sbatch(
                [
                    "-N", "1",
                    "-w", node,
                    "--exclusive",
                    "-c", "1",
                    "--gres=gpu:1",
                    "-t", "10",
                    holder_script,
                ]
            )
        )
        assert holder_id is not None, "GPU holder job did not submit"

        try:
            wait_job_state(cluster, holder_id, "R", timeout=60)

            # Second job wants 1 GPU on the same node — should be blocked.
            blocked_script = cluster.write_file(
                "excl-gpu-blocked.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
            )
            blocked_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-N", "1",
                        "-w", node,
                        "-c", "1",
                        "--gres=gpu:1",
                        "-t", "1",
                        blocked_script,
                    ]
                )
            )
            assert blocked_id is not None, "GPU blocked job did not submit"

            try:
                time.sleep(15)
                sq = cluster.squeue_all()
                lines = [l for l in sq.splitlines() if str(blocked_id) in l]
                assert lines, f"blocked GPU job {blocked_id} not found in squeue:\n{sq}"
                state = lines[0].split()[4] if len(lines[0].split()) > 4 else ""
                assert state == "PD", (
                    f"GPU job {blocked_id} should be PENDING while exclusive job "
                    f"{holder_id} holds the node with {total_gpus} GPUs, "
                    f"but state={state!r}:\n{sq}"
                )
            finally:
                cluster.scancel(str(blocked_id))
        finally:
            cluster.scancel(str(holder_id))

    def test_exclusive_node_released_after_job_completes(self, cluster):
        """After an exclusive job finishes, the node becomes available and a
        waiting job schedules without manual intervention."""
        node = cluster.node_names[0]

        # Short exclusive holder.
        holder_script = cluster.write_file(
            "excl-release-holder.sh", "#!/bin/bash\nsleep 10\n"
        )
        holder_id = parse_job_id(
            cluster.sbatch(
                [
                    "-N", "1",
                    "-w", node,
                    "--exclusive",
                    "-c", "1",
                    "-t", "2",
                    holder_script,
                ]
            )
        )
        assert holder_id is not None

        try:
            wait_job_state(cluster, holder_id, "R", timeout=60)

            # Waiter submitted while holder is running.
            out_path = f"{cluster.remote_dir}/excl-release-waiter.out"
            waiter_script = cluster.write_file(
                "excl-release-waiter.sh", "#!/bin/bash\necho RELEASED_OK\n"
            )
            waiter_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-N", "1",
                        "-w", node,
                        "-c", "1",
                        "-t", "1",
                        "-o", out_path,
                        waiter_script,
                    ]
                )
            )
            assert waiter_id is not None

            # Holder finishes, then waiter must complete.
            wait_job(cluster, holder_id, timeout=60)
            state = wait_job(cluster, waiter_id, timeout=60)
            assert state in ("CD", "GONE"), (
                f"waiter job {waiter_id} should complete after exclusive job "
                f"releases the node, but state={state!r}"
            )

            content = cluster.read_output_on_any_node(out_path)
            assert "RELEASED_OK" in content, (
                f"waiter output missing after node release:\n{content}"
            )
        finally:
            cluster.scancel(str(holder_id))
