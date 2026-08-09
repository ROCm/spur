# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for job dispatch under partial failure.

The scheduler awaits every node's LaunchJob before flipping a job to Running,
and settles a job back to Pending (rather than Failed) when a node cannot take
it. These are multi-node race conditions, so the assertions target the
observable invariants rather than internal timing.
"""

import re
import time

from cluster import (
    job_node_indices,
    job_state,
    parse_job_id,
    wait_job,
    wait_job_state,
)


def _launched_job_ids(cluster, node_index: int) -> set[int]:
    """Job IDs whose launch spurd confirmed on this node."""
    log = cluster.spurd_log(node_index)
    ids = set()
    for line in log.splitlines():
        if "job launched successfully" not in line:
            continue
        match = re.search(r"job_id[=:\s]+(\d+)", line)
        if match:
            ids.add(int(match.group(1)))
    return ids


class TestAllNodeDispatchConfirmation:
    def test_job_is_running_only_after_every_node_launched(self, multi_node_cluster):
        """A job must not be visibly Running while a node is still mid-launch.

        Consumers gate on Running before targeting a node, so the first moment
        squeue reports Running is the moment every allocated node must already
        have the job.
        """
        cluster = multi_node_cluster
        script = cluster.write_file("dispatch-confirm.sh", "#!/bin/bash\nsleep 120\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "dispatch-confirm", "-N", "2", script])
        )
        assert job_id is not None

        try:
            wait_job_state(cluster, job_id, "R", timeout=90)
            missing = [
                cluster.node_names[i]
                for i in job_node_indices(cluster, job_id)
                if job_id not in _launched_job_ids(cluster, i)
            ]
            assert not missing, (
                f"job {job_id} reported Running before {missing} confirmed its "
                f"launch\n{cluster.debug_job(job_id)}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_running_job_accepts_exec_immediately(self, multi_node_cluster):
        """No retry window: exec must succeed on the first attempt after Running."""
        cluster = multi_node_cluster
        script = cluster.write_file("dispatch-exec.sh", "#!/bin/bash\nsleep 120\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "dispatch-exec", "-N", "2", script])
        )
        assert job_id is not None

        try:
            wait_job_state(cluster, job_id, "R", timeout=90)
            code, out = cluster.spur_exec(job_id, ["true"])
            assert code == 0, (
                f"exec into job {job_id} failed on the first attempt after "
                f"Running:\n{out}\n{cluster.debug_job(job_id)}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestLaunchFailureBackoff:
    def test_job_stays_pending_when_target_node_agent_is_down(self, multi_node_cluster):
        """A non-prolog dispatch failure leaves the job Pending, not Failed.

        The job never left Pending, so there is no Running -> Failed detour to
        route the failure through; it must simply wait for the node to return.
        """
        cluster = multi_node_cluster
        target = cluster.node_names[0]

        cluster.stop_agent(0)
        time.sleep(2)

        script = cluster.write_file("backoff.sh", "#!/bin/bash\necho BACKOFF_RAN\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "backoff", "-N", "1", "-w", target, script])
        )
        assert job_id is not None

        try:
            # Give the scheduler several cycles to try and fail.
            time.sleep(15)
            state = job_state(cluster.squeue_all(), job_id)
            assert state == "PD", (
                f"job targeting a down node must stay Pending, got {state}\n"
                f"{cluster.debug_job(job_id)}"
            )

            cluster.restart_agent(0)
            cluster.wait_ready(timeout=90)

            assert wait_job(cluster, job_id, timeout=120) == "CD", (
                f"job must run once the node returns\n{cluster.debug_job(job_id)}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


# A late report finalizing a *newer* run attempt needs a job to run twice, which
# no CLI path produces today, so that race cannot be built from the outside.


class TestGpuReleaseOnLaunchFailure:
    # Fails only the job under test, so a later job can prove the GPUs came
    # back rather than being blocked by the same prolog.
    SELECTIVE_PROLOG = (
        "#!/bin/bash\n"
        'if [ "$SPUR_JOB_NAME" = "gpu-fail" ]; then exit 1; fi\n'
        "exit 0\n"
    )

    def test_gpus_are_released_when_launch_fails(self, unstarted_cluster):
        """A job aborted mid-launch must not leak its GPU reservation.

        `scontrol show node` reports configured GPUs, not free ones, so the
        leak is detected by whether a later job can claim every GPU on the node.
        """
        cluster = unstarted_cluster
        cluster.gpu_preflight(min_nodes=1)

        prolog = cluster.write_file(
            "selective-prolog.sh", self.SELECTIVE_PROLOG, all_nodes=True
        )
        cluster.start(
            config_overrides={
                "hooks": {"prolog": prolog},
                "devices": {"auto_detect": True},
            }
        )
        cluster.assert_sinfo_gpus(min_per_node=1)

        target = cluster.node_names[0]
        total_gpus = cluster.node_gpu_count(target)

        fail_script = cluster.write_file(
            "gpu-fail.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
        )
        failed_id = parse_job_id(
            cluster.sbatch(
                ["-J", "gpu-fail", "-N", "1", "-w", target, "--gpus", "1", fail_script]
            )
        )
        assert failed_id is not None

        deadline = time.time() + 60
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), failed_id) in ("PD", "F", "CA", None):
                break
            time.sleep(2)
        cluster.cli_allow_fail(["scancel", str(failed_id)])

        # The prolog failure drains the node; clear it so the probe job can run.
        cluster.cli_allow_fail(
            ["scontrol", "update", f"NodeName={target}", "State=RESUME"]
        )
        cluster.wait_ready(timeout=90)

        ok_script = cluster.write_file("gpu-ok.sh", "#!/bin/bash\necho GPU_OK\n")
        probe_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J", "gpu-ok",
                    "-N", "1",
                    "-w", target,
                    "--gpus", str(total_gpus),
                    ok_script,
                ]
            )
        )
        assert probe_id is not None

        try:
            assert wait_job(cluster, probe_id, timeout=120) == "CD", (
                f"a job claiming all {total_gpus} GPU(s) on {target} did not run, "
                f"so the failed launch leaked its reservation\n"
                f"{cluster.debug_job(probe_id)}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(probe_id)])
