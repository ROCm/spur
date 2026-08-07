# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for burst buffer capacity and staging.

`--bb` carries two independent things: a `capacity=` reservation the controller
holds against the `[burst_buffer]` pool, and `stage_in:`/`stage_out:` shell
commands the agent wraps around the job script. Capacity gating and command
wrapping are exercised separately because a spec can use either alone.

Stage-in completes synchronously inside the controller today, so the
`BurstBufferStageIn` hold is only observable for a scheduler tick and is not
asserted here.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

POOL_GB = 64


@pytest.fixture
def bb_cluster(unstarted_cluster):
    unstarted_cluster.start(config_overrides={"burst_buffer": {"total_gb": POOL_GB}})
    return unstarted_cluster


def pending_reason(cluster, job_id: int) -> str:
    out = cluster.scontrol("show", "job", str(job_id))
    match = re.search(r"Reason=(\S+)", out)
    return match.group(1) if match else ""


def wait_reason(cluster, job_id: int, reason: str, timeout: int = 60) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pending_reason(cluster, job_id) == reason:
            return
        time.sleep(2)
    raise AssertionError(
        f"job {job_id} never reported Reason={reason}\n"
        f"{cluster.scontrol('show', 'job', str(job_id))}"
    )


def submit_bb(cluster, name: str, bb: str, body: str, out_path: str | None = None) -> int:
    script = cluster.write_file(f"{name}.sh", f"#!/bin/bash\n{body}\n")
    args = ["-J", name, "--bb", bb]
    if out_path:
        args += ["-o", out_path]
    job_id = parse_job_id(cluster.sbatch(args + [script]))
    assert job_id is not None
    return job_id


class TestCapacityPool:
    def test_a_request_within_the_pool_runs(self, bb_cluster):
        job_id = submit_bb(bb_cluster, "bb-fit", "capacity=32", "echo BB_FIT_OK")
        assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            bb_cluster.debug_job(job_id)
        )

    def test_a_request_larger_than_the_pool_stays_pending(self, bb_cluster):
        job_id = submit_bb(
            bb_cluster, "bb-huge", f"capacity={POOL_GB * 4}", "echo unreachable"
        )
        try:
            wait_reason(bb_cluster, job_id, "BurstBufferResources")
        finally:
            bb_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_capacity_is_disabled_by_default(self, cluster):
        """With no `[burst_buffer]` section the pool is zero, so any capacity
        request must block rather than silently succeed unmetered."""
        job_id = submit_bb(cluster, "bb-nopool", "capacity=1", "echo unreachable")
        try:
            wait_reason(cluster, job_id, "BurstBufferResources")
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_a_second_job_waits_for_the_pool(self, bb_cluster):
        holder = submit_bb(
            bb_cluster, "bb-hold", f"capacity={POOL_GB}", "sleep 120"
        )
        wait_job_state(bb_cluster, holder, "R", timeout=90)

        waiter = submit_bb(bb_cluster, "bb-wait", "capacity=32", "echo unreachable")
        try:
            wait_reason(bb_cluster, waiter, "BurstBufferResources")
        finally:
            for job_id in (holder, waiter):
                bb_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_capacity_is_returned_when_the_holder_ends(self, bb_cluster):
        holder = submit_bb(bb_cluster, "bb-rel", f"capacity={POOL_GB}", "sleep 120")
        wait_job_state(bb_cluster, holder, "R", timeout=90)

        waiter = submit_bb(bb_cluster, "bb-next", f"capacity={POOL_GB}", "sleep 30")
        wait_reason(bb_cluster, waiter, "BurstBufferResources")

        bb_cluster.cli_allow_fail(["scancel", str(holder)])
        try:
            wait_job_state(bb_cluster, waiter, "R", timeout=120)
        finally:
            bb_cluster.cli_allow_fail(["scancel", str(waiter)])

    def test_a_malformed_capacity_does_not_reserve(self, bb_cluster):
        """An unparseable capacity reads as zero. That is permissive, but it
        must at least not wedge the job in a permanent pending state."""
        job_id = submit_bb(bb_cluster, "bb-bad", "capacity=abc", "echo BB_BAD_OK")
        assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            bb_cluster.debug_job(job_id)
        )


class TestStaging:
    def test_stage_in_runs_before_the_script(self, bb_cluster):
        marker = f"{bb_cluster.remote_dir}/bb-stage-in.txt"
        out_path = f"{bb_cluster.remote_dir}/bb-in.out"
        job_id = submit_bb(
            bb_cluster,
            "bb-in",
            f"stage_in:echo staged > {marker}",
            f"cat {marker}",
            out_path,
        )
        assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            bb_cluster.debug_job(job_id)
        )
        out = bb_cluster.read_output_on_any_node(out_path)
        assert "staged" in out, f"stage_in output not visible to the script:\n{out}"

    def test_stage_out_runs_after_the_script(self, bb_cluster):
        src = f"{bb_cluster.remote_dir}/bb-out-src.txt"
        dst = f"{bb_cluster.remote_dir}/bb-out-dst.txt"
        job_id = submit_bb(
            bb_cluster,
            "bb-out",
            f"stage_out:cp {src} {dst}",
            f"echo produced > {src}",
        )
        assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            bb_cluster.debug_job(job_id)
        )
        assert "produced" in bb_cluster.read_output_on_any_node(dst)

    def test_a_failing_stage_in_fails_the_job(self, bb_cluster):
        """Running the script against half-staged data would corrupt results,
        so stage-in is fail-fast."""
        marker = f"{bb_cluster.remote_dir}/bb-badin-ran.txt"
        job_id = submit_bb(
            bb_cluster,
            "bb-badin",
            "stage_in:cp /nonexistent/src /nonexistent/dst",
            f"echo ran > {marker}",
        )
        assert wait_job(bb_cluster, job_id, timeout=120) == "F", (
            bb_cluster.debug_job(job_id)
        )
        ran_on = [
            name
            for name, node in zip(bb_cluster.node_names, bb_cluster.nodes)
            if "RAN" in node.exec_allow_fail(
                f"test -e '{marker}' && echo RAN || echo ABSENT"
            )
        ]
        assert not ran_on, (
            f"stage-in is fail-fast, but the script still ran on {ran_on}"
        )

    def test_a_failing_stage_out_does_not_fail_the_job(self, bb_cluster):
        """Stage-out is best-effort: the job's real work is already done, so a
        copy failure must not retroactively mark it failed."""
        # A command that exits non-zero, not `exit`, which would terminate the
        # wrapper itself and prove nothing about stage-out being best-effort.
        job_id = submit_bb(
            bb_cluster,
            "bb-badout",
            "stage_out:cp /nonexistent/src /nonexistent/dst",
            "echo BB_OUT_OK",
        )
        assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            bb_cluster.debug_job(job_id)
        )

    def test_the_script_exit_code_survives_the_wrapper(self, bb_cluster):
        job_id = submit_bb(bb_cluster, "bb-exit", "stage_out:true", "exit 3")
        assert wait_job(bb_cluster, job_id, timeout=120) == "F", (
            bb_cluster.debug_job(job_id)
        )
        out = bb_cluster.scontrol("show", "job", str(job_id))
        assert "ExitCode=3" in out, out

    def test_the_spec_is_exported_to_the_job(self, bb_cluster):
        spec = "capacity=8;stage_in:true"
        out_path = f"{bb_cluster.remote_dir}/bb-env.out"
        job_id = submit_bb(
            bb_cluster, "bb-env", spec, 'echo "BB=$SPUR_BURST_BUFFER"', out_path
        )
        assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            bb_cluster.debug_job(job_id)
        )
        out = bb_cluster.read_output_on_any_node(out_path)
        assert f"BB={spec}" in out, out

    def test_staging_without_capacity_needs_no_pool(self, bb_cluster):
        """A stage-only spec parses to zero capacity, so it must dispatch even
        when the pool is fully reserved."""
        holder = submit_bb(bb_cluster, "bb-full", f"capacity={POOL_GB}", "sleep 120")
        wait_job_state(bb_cluster, holder, "R", timeout=90)
        try:
            job_id = submit_bb(
                bb_cluster, "bb-stageonly", "stage_in:true", "echo BB_STAGE_OK"
            )
            assert wait_job(bb_cluster, job_id, timeout=120) in ("CD", "GONE"), (
                bb_cluster.debug_job(job_id)
            )
        finally:
            bb_cluster.cli_allow_fail(["scancel", str(holder)])
