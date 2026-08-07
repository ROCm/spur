# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for job ownership enforcement on interactive entry points.

`spur exec`, `sattach` and output streaming all reach into a running job, so
each must reject a caller who does not own it. The identity travels from the
invoking UNIX account through the CLI to the controller, which is why these
run the CLI under a second account via sudo.
"""

import time

import pytest

from cluster import parse_job_id, wait_job_state


# root is an administrative override in check_job_owner, so a denial test has
# to run as an ordinary account instead.
NON_OWNER = "nobody"


def _require_second_identity(cluster) -> str:
    """Skip unless the environment can run the CLI as a second UNIX user."""
    submit_user = cluster.nodes[0].user
    if submit_user == NON_OWNER:
        pytest.skip(f"SSH user is {NON_OWNER}; need a different account to submit as")

    probe = cluster.cli_as_user(NON_OWNER, ["squeue", "-h"])
    if "sudo" in probe.lower() and (
        "password" in probe.lower() or "not allowed" in probe.lower()
    ):
        pytest.skip(f"sudo -u unavailable in this environment: {probe.strip()}")
    return submit_user


def _start_long_job(cluster, name: str) -> int:
    script = cluster.write_file(f"{name}.sh", "#!/bin/bash\nsleep 300\n")
    job_id = parse_job_id(cluster.sbatch(["-J", name, script]))
    assert job_id is not None
    wait_job_state(cluster, job_id, "R", timeout=60)
    return job_id


def _denied(output: str) -> bool:
    """Whether the controller refused the caller.

    "job owned by" is how AuthError::NotJobOwner renders; the other spellings
    cover refusals raised before the owner check.
    """
    lowered = output.lower()
    return (
        "job owned by" in lowered
        or "permission" in lowered
        or "denied" in lowered
        or "not authorized" in lowered
    )


class TestJobOwnership:
    def test_non_owner_cannot_exec_into_job(self, cluster):
        _require_second_identity(cluster)
        job_id = _start_long_job(cluster, "own-exec")
        try:
            out = cluster.cli_as_user(
                NON_OWNER, ["spur", "exec", str(job_id), "whoami"]
            )
            # A distinct, unprivileged identity, so the controller's owner
            # check must reject it.
            assert _denied(out), (
                f"a non-owner must be denied exec into job {job_id}, got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_non_owner_cannot_attach_to_job(self, cluster):
        _require_second_identity(cluster)
        # The owner check lives on the agent, so the denial is only observable
        # once the client can reach it.
        cluster.agent_resolution_preflight()
        job_id = _start_long_job(cluster, "own-attach")
        try:
            out = cluster.cli_as_user(NON_OWNER, ["sattach", str(job_id)])
            assert _denied(out), (
                f"a non-owner must be denied attach to job {job_id}, got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_non_owner_cannot_stream_job_output(self, cluster):
        _require_second_identity(cluster)
        cluster.agent_resolution_preflight()
        job_id = _start_long_job(cluster, "own-stream")
        try:
            out = cluster.cli_as_user(
                NON_OWNER, ["sattach", str(job_id), "--output-only"]
            )
            assert _denied(out), (
                f"a non-owner must be denied output streaming for job {job_id}, "
                f"got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_owner_can_exec_into_own_job(self, cluster):
        """The owner path must still work, so the denial above is about
        identity rather than a broken exec."""
        submit_user = _require_second_identity(cluster)
        job_id = _start_long_job(cluster, "own-exec-ok")
        try:
            code, out = cluster.spur_exec(job_id, ["whoami"])
            assert code == 0, f"owner exec into job {job_id} failed:\n{out}"
            assert submit_user in out, (
                f"exec must run as the job owner {submit_user}, got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_exec_rejects_unknown_job(self, cluster):
        code, out = cluster.spur_exec(999999, ["whoami"])
        assert code != 0, f"exec into a nonexistent job must fail, got:\n{out}"
        assert "not found" in out.lower(), f"expected a not-found error, got:\n{out}"

    def test_exec_rejects_job_that_is_not_running(self, cluster):
        script = cluster.write_file("own-pending.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "own-pending", "-H", script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "PD", timeout=30)
        try:
            code, out = cluster.spur_exec(job_id, ["whoami"])
            assert code != 0, f"exec into a held job must fail, got:\n{out}"
            assert "not running" in out.lower(), (
                f"expected a not-running error, got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestRestSubmittedJobOwnership:
    """A REST submission may omit `user`, leaving the job with no owner.

    Such a job runs as root, so every non-root caller must be denied rather
    than inheriting access to a root-owned process.
    """

    def test_non_root_denied_on_unowned_job(self, cluster):
        submit_user = _require_second_identity(cluster)
        cluster.curl_preflight()

        script = "#!/bin/bash\nsleep 300\n"
        body = (
            '{"job":{"name":"unowned","script":'
            f'"{script.encode("unicode_escape").decode()}"'
            ',"nodes":1,"ntasks":1}}'
        )
        status, resp = cluster.http_post("/api/v1/job/submit", body)
        if status != 200:
            pytest.skip(f"REST submit unavailable (status {status}): {resp}")

        job_id = None
        deadline = time.time() + 30
        while time.time() < deadline and job_id is None:
            ids = cluster.running_job_ids_by_name("unowned")
            job_id = ids[0] if ids else None
            if job_id is None:
                time.sleep(2)
        assert job_id is not None, (
            f"REST-submitted job never started:\n{cluster.squeue_all()}"
        )

        try:
            out = cluster.cli_as_user(
                submit_user, ["spur", "exec", str(job_id), "whoami"]
            )
            assert _denied(out), (
                f"a non-root caller must be denied exec into an unowned job, "
                f"got:\n{out}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])
