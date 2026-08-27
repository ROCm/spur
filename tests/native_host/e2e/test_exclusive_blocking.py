# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests that verify exclusive jobs fence their node against co-scheduling.

The specific scenario covered: an exclusive job that requests only 1 CPU (and
optionally 1 GPU) out of a node's full capacity must prevent the backfill
scheduler from placing any other job on that node while the exclusive job runs,
regardless of how many resources remain nominally free.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def _expand_nodelist(nodelist: str) -> list:
    """Expand a Slurm compressed hostlist into individual node names.

    Handles plain names, comma-separated lists, and bracket notation like
    ``node[1-3,5]`` → [node1, node2, node3, node5].  Commas inside brackets
    are treated as range separators, not top-level delimiters.
    """
    # Split on commas that are outside brackets.
    parts = re.split(r",(?![^\[]*\])", nodelist)
    nodes = []
    for part in parts:
        part = part.strip()
        m = re.match(r"^(.*)\[([^\]]+)\](.*)$", part)
        if not m:
            nodes.append(part)
            continue
        prefix, ranges, suffix = m.group(1), m.group(2), m.group(3)
        for token in ranges.split(","):
            token = token.strip()
            if "-" in token:
                lo, hi = token.split("-", 1)
                width = len(lo)
                for n in range(int(lo), int(hi) + 1):
                    nodes.append(f"{prefix}{str(n).zfill(width)}{suffix}")
            else:
                nodes.append(f"{prefix}{token}{suffix}")
    return nodes


# How long to keep polling after the holder is running before declaring the
# blocked job incorrectly scheduled. The backfill interval in e2e tests is
# 1 second, so 10 seconds covers at least 10 full scheduler passes.
_BLOCKING_POLL_SECS = 10
_BLOCKING_POLL_STEP = 1


def _job_state_and_reason(cluster, job_id: int) -> tuple[str, str]:
    """Return (state, reason) for job_id using squeue %t and %r format fields.

    Returns ("", "") when the job is not found (already gone).
    """
    out = cluster.squeue(["-j", str(job_id), "-h", "-o", "%t %r"])
    out = out.strip()
    if not out:
        return "", ""
    parts = out.split(None, 1)
    state = parts[0] if parts else ""
    reason = parts[1] if len(parts) > 1 else ""
    return state, reason


def _assert_all_stay_pending_for_resources(
    cluster, job_ids: list, holder_id: int, duration: int
):
    """Poll for `duration` seconds and assert every job in job_ids stays PD
    with reason Resources throughout.

    All jobs are checked on every tick so that a job running and completing
    while another is being examined cannot slip through undetected. Fails
    immediately the moment any job starts running or shows a wrong reason.
    Requires every job to have been seen as PD at least once.
    """
    deadline = time.time() + duration
    seen = {jid: False for jid in job_ids}

    while time.time() < deadline:
        for jid in job_ids:
            state, reason = _job_state_and_reason(cluster, jid)

            if state == "R":
                sq = cluster.squeue_all()
                raise AssertionError(
                    f"Job {jid} started running while exclusive job {holder_id} "
                    f"holds the node — exclusive blocking is broken.\n{sq}"
                )

            if state == "PD":
                seen[jid] = True
                assert reason == "Resources", (
                    f"Job {jid} is PENDING but for the wrong reason: {reason!r}. "
                    f"Expected 'Resources' (blocked by exclusive holder {holder_id}). "
                    f"This may indicate a misconfigured partition or a different "
                    f"scheduling issue masking the real block."
                )

        time.sleep(_BLOCKING_POLL_STEP)

    for jid, was_seen in seen.items():
        assert was_seen, (
            f"Job {jid} never appeared as PD in squeue during the {duration}s window — "
            f"it may have completed instantly or never been queued properly."
        )


class TestExclusiveBlocking:
    def test_exclusive_cpu_blocks_coscheduling(self, cluster):
        """An exclusive job requesting 1 CPU blocks a second job from landing on
        the same node, even though 63 of 64 CPUs are nominally free.

        The blocked job must stay PD with reason=Resources (not Priority or
        any other reason) for the duration of multiple backfill passes.
        """
        node = cluster.node_names[0]

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

            # -c 1 is trivially satisfiable on an idle node, so Resources is the
            # only legitimate reason to pend — it proves the exclusive hold is
            # blocking the scheduler, not a resource shortage or priority issue.
            blocked_script = cluster.write_file(
                "excl-blocked.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
            )
            blocked_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-N", "1",
                        "-w", node,
                        "-c", "1",
                        "-t", "1",
                        blocked_script,
                    ]
                )
            )
            assert blocked_id is not None, "blocked job did not submit"

            try:
                _assert_all_stay_pending_for_resources(
                    cluster, [blocked_id], holder_id, _BLOCKING_POLL_SECS
                )
            finally:
                cluster.scancel(str(blocked_id))
        finally:
            cluster.scancel(str(holder_id))

    def test_exclusive_gpu_blocks_coscheduling(self, gpu_cluster):
        """An exclusive job requesting 1 CPU and 1 GPU blocks a second GPU job
        from landing on the same node, even though most GPUs are nominally free.

        The blocked job must stay PD with reason=Resources across multiple
        backfill passes.
        """
        cluster = gpu_cluster
        cluster.gpu_preflight(1)

        node = cluster.node_names[0]
        total_gpus = cluster.node_gpu_count(node)
        if total_gpus < 2:
            pytest.skip(
                f"need >= 2 GPUs on {node} to prove remaining GPUs are blocked "
                f"(found {total_gpus})"
            )

        # Exclusive holder: 1 CPU, 1 GPU — leaving total_gpus-1 GPUs nominally free.
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
                _assert_all_stay_pending_for_resources(
                    cluster, [blocked_id], holder_id, _BLOCKING_POLL_SECS
                )
            finally:
                cluster.scancel(str(blocked_id))
        finally:
            cluster.scancel(str(holder_id))

    def test_exclusive_node_released_after_job_completes(self, cluster):
        """After an exclusive job finishes, the node becomes available and a
        waiting job schedules and completes without manual intervention."""
        node = cluster.node_names[0]

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

            # Confirm waiter is blocked with the right reason before holder exits.
            state, reason = _job_state_and_reason(cluster, waiter_id)
            if state == "PD":
                assert reason == "Resources", (
                    f"waiter {waiter_id} is PD for wrong reason {reason!r} "
                    f"before holder finishes"
                )

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

    def test_exclusive_multinode_blocks_coscheduling(self, cluster):
        """An exclusive 2-node job requesting 1 CPU per node blocks a third job
        from landing on either node, even though 63 CPUs per node are free.

        Requires at least 2 nodes in SPUR_TEST_NODES.
        """
        cluster.require_nodes(2)

        holder_script = cluster.write_file(
            "excl-mn-holder.sh", "#!/bin/bash\nsleep 300\n"
        )
        holder_id = parse_job_id(
            cluster.sbatch(
                [
                    "-N", "2",
                    "--exclusive",
                    "-c", "1",
                    "-t", "10",
                    holder_script,
                ]
            )
        )
        assert holder_id is not None, "multi-node exclusive holder did not submit"

        try:
            wait_job_state(cluster, holder_id, "R", timeout=60)

            # Read the nodes the holder actually landed on — do not assume
            # node_names[0]/[1] match the allocated nodes. The cluster may have
            # a controller-only node that the scheduler skips.
            sq = cluster.squeue(["-j", str(holder_id), "-h", "-o", "%D %N"])
            parts = sq.strip().split(None, 1)
            assert len(parts) == 2, f"unexpected squeue output for holder: {sq!r}"
            assert parts[0] == "2", (
                f"holder should be allocated 2 nodes, got: {sq.strip()!r}"
            )
            allocated_nodes = _expand_nodelist(parts[1].strip())
            assert len(allocated_nodes) == 2, (
                f"expected 2 allocated nodes, got {allocated_nodes}"
            )

            # Submit one blocked job per allocated node before polling either,
            # so a failure on one node is not masked while we watch the other.
            blocked_ids = []
            for i, node in enumerate(allocated_nodes):
                script = cluster.write_file(
                    f"excl-mn-blocked{i}.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
                )
                bid = parse_job_id(
                    cluster.sbatch(
                        ["-N", "1", "-w", node, "-c", "1", "-t", "1", script]
                    )
                )
                assert bid is not None, f"blocked job {i} did not submit"
                blocked_ids.append(bid)

            try:
                _assert_all_stay_pending_for_resources(
                    cluster, blocked_ids, holder_id, _BLOCKING_POLL_SECS
                )
            finally:
                for bid in blocked_ids:
                    cluster.scancel(str(bid))
        finally:
            cluster.scancel(str(holder_id))

    def test_exclusive_multinode_gpu_blocks_coscheduling(self, gpu_cluster):
        """An exclusive 2-node GPU job requesting 1 GPU per node blocks further
        GPU jobs from landing on either node, even though most GPUs are free.

        Requires at least 2 nodes in SPUR_TEST_NODES.
        """
        cluster = gpu_cluster
        cluster.require_nodes(2)
        cluster.gpu_preflight(2)

        holder_script = cluster.write_file(
            "excl-mn-gpu-holder.sh", "#!/bin/bash\nsleep 300\n"
        )
        holder_id = parse_job_id(
            cluster.sbatch(
                [
                    "-N", "2",
                    "--exclusive",
                    "-c", "1",
                    "--gres=gpu:1",
                    "-t", "10",
                    holder_script,
                ]
            )
        )
        assert holder_id is not None, "multi-node GPU exclusive holder did not submit"

        try:
            wait_job_state(cluster, holder_id, "R", timeout=60)

            # Read the nodes the holder actually landed on.
            sq = cluster.squeue(["-j", str(holder_id), "-h", "-o", "%D %N"])
            parts = sq.strip().split(None, 1)
            assert len(parts) == 2, f"unexpected squeue output for holder: {sq!r}"
            assert parts[0] == "2", (
                f"holder should be allocated 2 nodes, got: {sq.strip()!r}"
            )
            allocated_nodes = _expand_nodelist(parts[1].strip())
            assert len(allocated_nodes) == 2, (
                f"expected 2 allocated nodes, got {allocated_nodes}"
            )

            for name in allocated_nodes:
                if cluster.node_gpu_count(name) < 2:
                    pytest.skip(
                        f"need >= 2 GPUs on each node to prove remaining GPUs are blocked "
                        f"(node {name} has {cluster.node_gpu_count(name)})"
                    )

            blocked_ids = []
            for i, node in enumerate(allocated_nodes):
                script = cluster.write_file(
                    f"excl-mn-gpu-blocked{i}.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
                )
                bid = parse_job_id(
                    cluster.sbatch(
                        ["-N", "1", "-w", node, "-c", "1", "--gres=gpu:1", "-t", "1",
                         script]
                    )
                )
                assert bid is not None, f"GPU blocked job {i} did not submit"
                blocked_ids.append(bid)

            try:
                _assert_all_stay_pending_for_resources(
                    cluster, blocked_ids, holder_id, _BLOCKING_POLL_SECS
                )
            finally:
                for bid in blocked_ids:
                    cluster.scancel(str(bid))
        finally:
            cluster.scancel(str(holder_id))
