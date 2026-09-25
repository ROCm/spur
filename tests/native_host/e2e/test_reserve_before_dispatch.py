# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for reserve-before-dispatch across a controller restart.

The controller now records a job's placement in the Raft log before
dispatching the launch RPC, so a leader change mid-dispatch finds the slice
already charged rather than free. The leader-local dispatch tracker that
tells a genuinely-abandoned reservation from one still in flight is never
persisted, so a freshly-restarted controller starts with an empty one; it
must not treat every reservation it doesn't yet know about as orphaned, but
it must still eventually free one that really is.

A controller restart reproduces the same empty-tracker condition a real
leadership change does (state recovered from the Raft log, dispatch_tracker
starts empty either way), without needing multi-controller Raft failover
support this harness does not yet have.
"""

import re
import time

import pytest

from cluster import block_agent_port, parse_job_id, wait_job_state

# Comfortably longer than a controller restart takes to answer queries again,
# so "still inside the grace window" isn't racing the restart itself.
LONG_GRACE_SECS = 60
# Short enough to observe elapsing without a slow test.
SHORT_GRACE_SECS = 3
PROBE_RUNNING_BOUND = 20


def _job_fields(cluster, job_id: int) -> dict[str, str]:
    out = cluster.scontrol("show", "job", str(job_id))
    return dict(re.findall(r"(\w+)=(\S+)", out))


def _wait_reserved(cluster, job_id: int, timeout: int = 30) -> None:
    """Poll until the job shows a charged placement while still Pending."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        fields = _job_fields(cluster, job_id)
        if (
            fields.get("JobState") == "PENDING"
            and fields.get("NodeList", "(null)") != "(null)"
        ):
            return
        time.sleep(1)
    raise TimeoutError(
        f"job {job_id} never showed a reserved-but-pending placement within "
        f"{timeout}s: {_job_fields(cluster, job_id)}"
    )


class TestReservationSurvivesRestartWithinGrace:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"dispatch_timeout_secs": LONG_GRACE_SECS}}

    def test_reservation_survives_a_restart_within_the_dispatch_grace_window(
        self, cluster
    ):
        script = cluster.write_file(
            "reserve-survives.sh", "#!/bin/bash\nsleep 60\n"
        )
        job_id = None
        with block_agent_port(cluster, node_index=0):
            try:
                sb = cluster.sbatch(
                    ["-J", "reserve-survives", "-N", "1",
                     "-w", cluster.node_names[0], script]
                )
                job_id = parse_job_id(sb)
                assert job_id is not None, f"sbatch failed: {sb}"

                _wait_reserved(cluster, job_id)
                reserved_nodelist = _job_fields(cluster, job_id)["NodeList"]

                cluster.restart_controller()

                fields = _job_fields(cluster, job_id)
                assert fields.get("JobState") == "PENDING", (
                    "a freshly-restarted controller must not have already "
                    f"decided this job's fate: {fields}"
                )
                assert fields.get("NodeList") == reserved_nodelist, (
                    "the reservation must survive the restart while inside "
                    f"the dispatch grace window, got: {fields}"
                )
            finally:
                if job_id is not None:
                    cluster.scancel(job_id)


class TestReservationReleasedAfterGraceElapses:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"controller": {"dispatch_timeout_secs": SHORT_GRACE_SECS}}

    def test_reservation_is_released_once_the_dispatch_grace_elapses(
        self, cluster
    ):
        script = cluster.write_file(
            "reserve-released.sh", "#!/bin/bash\nsleep 60\n"
        )
        probe_script = cluster.write_file(
            "reserve-released-probe.sh", "#!/bin/bash\nsleep 60\n"
        )
        submitted = []
        try:
            with block_agent_port(cluster, node_index=0):
                sb = cluster.sbatch(
                    ["-J", "reserve-released", "-N", "1",
                     "-w", cluster.node_names[0], script]
                )
                job_id = parse_job_id(sb)
                assert job_id is not None, f"sbatch failed: {sb}"
                submitted.append(job_id)

                _wait_reserved(cluster, job_id)
                cluster.restart_controller()

            # Block lifted (context manager exited): a redispatch to this
            # node can now actually succeed once the sweep frees it.
            probe_sb = cluster.sbatch(
                ["-J", "reserve-released-probe", "-N", "1",
                 "-w", cluster.node_names[0], probe_script]
            )
            probe_job = parse_job_id(probe_sb)
            assert probe_job is not None, f"sbatch failed: {probe_sb}"
            submitted.append(probe_job)

            wait_job_state(
                cluster, probe_job, "R",
                timeout=SHORT_GRACE_SECS + PROBE_RUNNING_BOUND,
            )
        finally:
            for jid in submitted:
                cluster.scancel(jid)
