# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for salloc allocation → srun step identity (SPUR-220).

When JWT auth binds a job to the token subject at submit time, step RPCs must
still succeed inside the allocation shell even after ``SPUR_AUTH_TOKEN`` is
unset — via ``SPUR_JOB_USER`` from salloc and/or a ``GetJob`` owner lookup.
"""

import os

import pytest

from cluster import parse_job_id, wait_job_state

# Must exist in NSS on the test nodes and differ from the typical SSH login.
JWT_OWNER = "root"

JWT_AUTH_CONFIG = {
    "auth": {"plugin": "jwt", "jwt_key": "e2e-salloc-step-auth-key"},
}


def _ssh_user() -> str:
    user = os.environ.get("SPUR_TEST_SSH_USER", "")
    assert user, "SPUR_TEST_SSH_USER must be set"
    return user


def _token_for(cluster, user: str) -> str:
    out = cluster.cli(
        [
            "spur",
            "token",
            "user",
            f"--user={user}",
            f"--config={cluster.etc_dir}/spur.conf",
        ]
    )
    token = out.strip().split("\n")[0]
    assert token.count(".") == 2, f"unexpected token format: {out}"
    return token


def _denied(combined: str) -> bool:
    lower = combined.lower()
    return (
        "cannot run a step" in lower
        or "cannot attach" in lower
        or "permission denied" in lower
        or "not job owner" in lower
    )


class TestSallocStepAuthJwt:
    """JWT permissive: submit identity can differ from the local login name."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return JWT_AUTH_CONFIG

    def test_salloc_exports_job_user(self, cluster):
        token = _token_for(cluster, JWT_OWNER)
        out_path = f"{cluster.remote_dir}/salloc-env.txt"
        body = f"""
unset SPUR_AUTH_TOKEN
echo "$SPUR_JOB_USER" > '{out_path}'
echo "$SLURM_JOB_USER" >> '{out_path}'
"""
        code, out = cluster.salloc_run(
            body,
            extra_env={"SPUR_AUTH_TOKEN": token},
        )
        assert code == 0, f"salloc failed (exit {code}):\n{out}"

        lines = cluster.nodes[0].read_file(out_path).strip().splitlines()
        assert len(lines) >= 2, f"expected SPUR/SLURM job user lines, got: {lines!r}"
        assert lines[0] == JWT_OWNER, f"SPUR_JOB_USER must be the JWT owner, got {lines[0]!r}"
        assert lines[1] == JWT_OWNER, f"SLURM_JOB_USER must mirror owner, got {lines[1]!r}"

    def test_srun_step_after_salloc_without_token(self, cluster):
        token = _token_for(cluster, JWT_OWNER)
        out_path = f"{cluster.remote_dir}/salloc-step.out"
        body = f"""
unset SPUR_AUTH_TOKEN
'{cluster.bin_dir}/srun' hostname > '{out_path}' 2>&1
"""
        code, out = cluster.salloc_run(
            body,
            extra_env={"SPUR_AUTH_TOKEN": token},
        )
        assert code == 0, f"salloc failed (exit {code}):\n{out}"

        step_out = cluster.nodes[0].read_file(out_path)
        assert not _denied(step_out), f"srun step must succeed inside salloc shell:\n{step_out}"
        assert step_out.strip(), f"expected hostname output, got:\n{step_out}"

    def test_srun_step_resolves_owner_via_get_job(self, cluster):
        """Step mode with only SPUR_JOB_ID still finds the owner on the controller."""
        ssh_user = _ssh_user()
        token = _token_for(cluster, JWT_OWNER)
        hold = cluster.write_file("auth-hold.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.cli_as_user(
            ssh_user,
            ["sbatch", "-J", "auth-hold", "-t", "5", hold],
            extra_env={"SPUR_AUTH_TOKEN": token},
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id,
                ["hostname"],
                unset_env=["SPUR_AUTH_TOKEN"],
            )
            assert code == 0, f"srun step failed (exit {code}):\n{out}"
            assert not _denied(out), out
            assert out.strip(), f"expected hostname output, got:\n{out}"
        finally:
            cluster.scancel(str(job_id))

    def test_srun_step_wrong_job_user_denied(self, cluster):
        ssh_user = _ssh_user()
        token = _token_for(cluster, JWT_OWNER)
        hold = cluster.write_file("auth-hold-deny.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.cli_as_user(
            ssh_user,
            ["sbatch", "-J", "auth-hold-deny", "-t", "5", hold],
            extra_env={"SPUR_AUTH_TOKEN": token},
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id,
                ["hostname"],
                extra_env={"SPUR_JOB_USER": "not-the-job-owner"},
                unset_env=["SPUR_AUTH_TOKEN"],
            )
            assert code != 0, f"step with a spoofed job user must fail:\n{out}"
            assert _denied(out), f"expected ownership denial, got:\n{out}"
        finally:
            cluster.scancel(str(job_id))

    def test_srun_step_jwt_non_owner_denied(self, cluster):
        """A JWT for a different subject cannot run a step in another user's job."""
        ssh_user = _ssh_user()
        if ssh_user == JWT_OWNER:
            pytest.skip("SSH user is root; need a distinct non-owner account")

        owner_token = _token_for(cluster, JWT_OWNER)
        caller_token = _token_for(cluster, ssh_user)
        hold = cluster.write_file("auth-hold-jwt.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.cli_as_user(
            ssh_user,
            ["sbatch", "-J", "auth-hold-jwt", "-t", "5", hold],
            extra_env={"SPUR_AUTH_TOKEN": owner_token},
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id,
                ["hostname"],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert code != 0, f"non-owner JWT must not run a step:\n{out}"
            assert _denied(out), f"expected ownership denial, got:\n{out}"
        finally:
            cluster.scancel(str(job_id))


class TestSallocStepAuthUidFallback:
    """Unauthenticated step RPCs may match the job via submit-time uid."""

    def test_srun_step_uid_fallback_with_wrong_username(self, cluster):
        hold = cluster.write_file("uid-hold.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-J", "uid-hold", "-t", "5", hold])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        wait_job_state(cluster, job_id, "R", timeout=60)
        try:
            code, out = cluster.srun_in_allocation(
                job_id,
                ["hostname"],
                extra_env={"SPUR_JOB_USER": "definitely-not-the-owner"},
            )
            assert code == 0, (
                f"uid fallback must allow a step when caller uid matches submit uid "
                f"(exit {code}):\n{out}"
            )
            assert not _denied(out), out
            assert out.strip(), f"expected hostname output, got:\n{out}"
        finally:
            cluster.scancel(str(job_id))
