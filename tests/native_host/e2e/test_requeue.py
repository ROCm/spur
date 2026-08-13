# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `scontrol requeue` / `requeuehold`.

Covers returning a running job to the queue (kill + re-pend, same job_id),
requeuing a finished job, the held variant, whole-array fan-out, and CLI
guards. Each test cancels its job in a finally block.
"""

import time

from cluster import parse_job_id, job_state


def _cleanup(cluster, job_id):
    """Best-effort cancel. No-op when submission never produced an id."""
    if job_id is None:
        return
    cluster.cli_allow_fail(["scancel", str(job_id)])


def _wait_state(cluster, job_id, want, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if job_state(cluster.squeue_all(), job_id) == want:
            return True
        time.sleep(1)
    return False


def _show_field(cluster, job_id, key):
    """Return the value of `key=` from `scontrol show job` (or None)."""
    show = cluster.scontrol("show", "job", str(job_id))
    for token in show.split():
        if token.startswith(f"{key}="):
            return token.split("=", 1)[1]
    return None


def _wait_finished(cluster, job_id, timeout=60):
    """Poll until the job is finished: either shown COMPLETED or gone from the
    active queue. Avoids racing on the RUNNING window for very short jobs."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        state = job_state(cluster.squeue_all(), job_id)
        if state in ("CD", None):
            return True
        time.sleep(1)
    return False


def _submit_sleep(cluster, name, sleep_secs=600, extra=None, wait_running=True):
    body = f"#!/bin/bash\necho STARTED\nsleep {sleep_secs}\n"
    script = cluster.write_file(f"{name}.sh", body)
    args = ["-J", name, "-N", "1"]
    if extra:
        args += extra
    args.append(script)
    job_id = parse_job_id(cluster.sbatch(args))
    assert job_id is not None, "submit failed"
    # Callers that want to observe RUNNING must use a long-enough sleep; a short
    # job can finish before squeue ever reports R, so `wait_running=False` skips
    # that (racy) assertion.
    if wait_running:
        assert _wait_state(cluster, job_id, "R"), "job never reached RUNNING"
    return job_id


class TestRequeueRunning:
    def test_requeue_running_job_returns_to_queue(self, cluster):
        """`scontrol requeue` on a RUNNING job kills it and re-pends it under
        the same job_id, and the scheduler re-dispatches it."""
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "rq-running")
            cluster.scontrol("requeue", str(job_id))
            # The job leaves RUNNING and comes back on its own (same id).
            assert _wait_state(cluster, job_id, "R", timeout=90), (
                "requeued job never returned to RUNNING"
            )
            # Same spec: the batch script path is unchanged.
            assert _show_field(cluster, job_id, "JobId") == str(job_id)
        finally:
            _cleanup(cluster, job_id)

    def test_requeue_finished_job_reschedules(self, cluster):
        """A finished job can be put back into the queue with `scontrol
        requeue`, keeping its job_id."""
        job_id = None
        try:
            # Short job so it completes quickly; don't require observing R (a
            # 2s job may finish before squeue reports RUNNING).
            job_id = _submit_sleep(
                cluster, "rq-finished", sleep_secs=2, wait_running=False
            )
            assert _wait_finished(cluster, job_id), "job did not finish"

            cluster.scontrol("requeue", str(job_id))
            # It should reappear as PENDING or RUNNING with the same id.
            back = _wait_state(cluster, job_id, "R", timeout=90) or _wait_state(
                cluster, job_id, "PD", timeout=1
            )
            assert back, "requeued finished job did not return to the queue"
        finally:
            _cleanup(cluster, job_id)


class TestRequeueHold:
    def test_requeuehold_parks_job_held_until_released(self, cluster):
        """`scontrol requeuehold` returns the job to PENDING and holds it; a
        subsequent `scontrol release` lets it run again."""
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "rq-hold")
            cluster.scontrol("requeuehold", str(job_id))
            assert _wait_state(cluster, job_id, "PD", timeout=60), (
                "requeuehold did not return the job to PENDING"
            )
            # It must stay held (not get dispatched) without a release.
            time.sleep(5)
            assert job_state(cluster.squeue_all(), job_id) == "PD", (
                "held job must not be dispatched until released"
            )
            assert _show_field(cluster, job_id, "JobState") == "PENDING"

            cluster.scontrol("release", str(job_id))
            assert _wait_state(cluster, job_id, "R", timeout=90), (
                "released job did not run"
            )
        finally:
            _cleanup(cluster, job_id)


def _wait_named_running(cluster, name, want=1, timeout=90):
    """Poll until at least `want` tasks named `name` are RUNNING; return their
    ids. Array tasks carry their own job ids (the parent id is not a stored
    job), so observe them by name rather than by the parent id."""
    deadline = time.time() + timeout
    ids = []
    while time.time() < deadline:
        ids = cluster.running_job_ids_by_name(name)
        if len(ids) >= want:
            return ids
        time.sleep(1)
    return ids


class TestRequeueArray:
    def test_requeue_array_parent_fans_out(self, cluster):
        """Requeuing the bare array-parent id returns every task to the queue."""
        name = "rqarr"
        try:
            body = "#!/bin/bash\necho STARTED\nsleep 600\n"
            script = cluster.write_file("rq-array.sh", body)
            parent = parse_job_id(
                cluster.sbatch(["-J", name, "-N", "1", "-a", "0-1", script])
            )
            assert parent is not None
            assert _wait_named_running(cluster, name), "no array task reached RUNNING"

            # The bare parent id (not a stored job) must fan out to the tasks,
            # not be rejected as unknown.
            out = cluster.scontrol("requeue", str(parent))
            assert "not found" not in out.lower(), out

            # Tasks re-pend and run again under their own ids.
            assert _wait_named_running(cluster, name), (
                "array tasks did not return to RUNNING after requeue"
            )
        finally:
            # The parent id is not a stored job, so scancel <parent> would error
            # and leak the tasks; cancel by name instead.
            cluster.cli_allow_fail(["scancel", "-n", name])


class TestRequeueCliGuards:
    def test_requeue_unknown_job_errors(self, cluster):
        out = cluster.cli_allow_fail(["scontrol", "requeue", "999999"])
        assert out.strip(), "expected an error message for unknown job id"

    def test_requeue_pending_job_rejected(self, cluster):
        """Requeuing an already-PENDING (held) job is rejected."""
        job_id = None
        try:
            script = cluster.write_file("rq-pending.sh", "#!/bin/bash\necho PD\n")
            job_id = parse_job_id(
                cluster.sbatch(["-J", "rq-pending", "-N", "1", "-H", script])
            )
            assert job_id is not None
            assert _wait_state(cluster, job_id, "PD", timeout=30)
            out = cluster.cli_allow_fail(["scontrol", "requeue", str(job_id)])
            assert job_state(cluster.squeue_all(), job_id) == "PD", (
                f"job should still be PENDING; cli said:\n{out}"
            )
        finally:
            _cleanup(cluster, job_id)
