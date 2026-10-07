# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for ``[auth] private_data`` (Slurm ``PrivateData`` parity).

Default (unset): every authenticated user sees every job and every assoc_mgr
scope, like a stock ``slurm.conf``. ``private_data = ["jobs"]`` hides other
users' jobs from ``squeue`` / ``scontrol show job``; ``private_data = ["usage"]``
pins ``scontrol show assoc_mgr`` to the scopes the caller takes part in.

JWT auth gives each caller a distinct identity via ``SPUR_AUTH_TOKEN`` so the
submit owner can differ from the invoking login account.
"""

import pytest

from cluster import parse_job_id, retry_until_qos_ready, wait_job_state

# Must exist in NSS on the test nodes; the job owner, distinct from the login.
JOB_OWNER = "root"

JWT_KEY = "e2e-private-data-key"


def _jwt_auth(private_data=None, cluster_admins=None):
    auth = {"plugin": "jwt", "jwt_key": JWT_KEY}
    if private_data is not None:
        auth["private_data"] = private_data
    if cluster_admins is not None:
        auth["cluster_admins"] = cluster_admins
    return {"auth": auth}


def _token_for(cluster, user: str) -> str:
    out = cluster.cli(
        ["spur", "token", "user", f"--user={user}", f"--config={cluster.etc_dir}/spur.conf"]
    )
    token = out.strip().split("\n")[0]
    assert token.count(".") == 2, f"unexpected token format: {out}"
    return token


def _login(cluster) -> str:
    login = cluster.nodes[0].user
    if login == JOB_OWNER:
        pytest.skip("SSH login is root; need a distinct non-owner account")
    return login


def _cancel_as_owner(cluster, job_id: int) -> None:
    cluster.cli_as_user(
        _login(cluster),
        ["scancel", str(job_id)],
        extra_env={"SPUR_AUTH_TOKEN": _token_for(cluster, JOB_OWNER)},
    )


def _submit_held_job_as_owner(cluster, name: str) -> int:
    """Submit a long-running job owned by ``JOB_OWNER`` and wait until it runs."""
    owner_token = _token_for(cluster, JOB_OWNER)
    script = cluster.write_file(f"{name}.sh", "#!/bin/bash\nsleep 120\n")
    out = cluster.cli_as_user(
        _login(cluster),
        ["sbatch", "-J", name, "-t", "5", script],
        extra_env={"SPUR_AUTH_TOKEN": owner_token},
    )
    job_id = parse_job_id(out)
    assert job_id is not None, f"sbatch failed: {out}"
    wait_job_state(cluster, job_id, "R", timeout=60)
    return job_id


class TestJobsPrivate:
    """``private_data = ["jobs"]``: a non-owner cannot see the owner's job."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _jwt_auth(private_data=["jobs"])

    def test_squeue_hides_other_users_jobs(self, cluster):
        login = _login(cluster)
        job_id = _submit_held_job_as_owner(cluster, "priv-hide")
        try:
            caller_token = _token_for(cluster, login)

            mine = cluster.cli_as_user(
                login,
                ["squeue", "-h", "-o", "%i %u"],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert str(job_id) not in mine, (
                f"{login} must not see {JOB_OWNER}'s job in squeue:\n{mine}"
            )

            # Filtering explicitly by the owner also comes back empty, not an error.
            by_owner = cluster.cli_as_user(
                login,
                ["squeue", "-h", "-o", "%i %u", "-u", JOB_OWNER],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert str(job_id) not in by_owner, (
                f"squeue -u {JOB_OWNER} must be empty for {login}:\n{by_owner}"
            )

            # An anonymous caller (permissive, no token) still sees every job.
            everyone = cluster.squeue(["-h", "-o", "%i %u"])
            assert str(job_id) in everyone, (
                f"an anonymous caller must see the job:\n{everyone}"
            )
        finally:
            _cancel_as_owner(cluster, job_id)

    def test_scontrol_show_job_reveals_nothing_to_a_non_owner(self, cluster):
        login = _login(cluster)
        job_id = _submit_held_job_as_owner(cluster, "priv-show")
        try:
            caller_token = _token_for(cluster, login)
            out = cluster.cli_as_user(
                login,
                ["scontrol", "show", "job", str(job_id)],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert f"JobId={job_id}" not in out, (
                f"scontrol show job must reveal nothing to a non-owner:\n{out}"
            )
        finally:
            _cancel_as_owner(cluster, job_id)


class TestJobsVisibleByDefault:
    """With ``private_data`` unset, every authenticated user sees every job."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _jwt_auth()

    def test_squeue_lists_other_users_jobs(self, cluster):
        login = _login(cluster)
        job_id = _submit_held_job_as_owner(cluster, "vis-list")
        try:
            caller_token = _token_for(cluster, login)

            mine = cluster.cli_as_user(
                login,
                ["squeue", "-h", "-o", "%i %u"],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert str(job_id) in mine, (
                f"{login} must see {JOB_OWNER}'s job by default:\n{mine}"
            )

            by_owner = cluster.cli_as_user(
                login,
                ["squeue", "-h", "-o", "%i %u", "-u", JOB_OWNER],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert str(job_id) in by_owner, (
                f"squeue -u {JOB_OWNER} must list the job by default:\n{by_owner}"
            )
        finally:
            _cancel_as_owner(cluster, job_id)

    def test_scontrol_show_job_succeeds_for_a_non_owner(self, cluster):
        login = _login(cluster)
        job_id = _submit_held_job_as_owner(cluster, "vis-show")
        try:
            caller_token = _token_for(cluster, login)
            out = cluster.cli_as_user(
                login,
                ["scontrol", "show", "job", str(job_id)],
                extra_env={"SPUR_AUTH_TOKEN": caller_token},
            )
            assert f"JobId={job_id}" in out, (
                f"a non-owner must read the job by default:\n{out}"
            )
        finally:
            _cancel_as_owner(cluster, job_id)


def _owner_job_in_qos(c, qos: str) -> int:
    """Run a job owned by ``JOB_OWNER`` under ``qos``, a scope the login user has no part in."""
    c.sacctmgr(["add", "qos", f"name={qos}"])
    script = c.write_file(f"{qos}.sh", "#!/bin/bash\nsleep 120\n")
    owner_token = _token_for(c, JOB_OWNER)
    out = retry_until_qos_ready(
        lambda: c.cli_as_user(
            _login(c),
            ["sbatch", "-J", qos, "-t", "5", f"--qos={qos}", script],
            extra_env={"SPUR_AUTH_TOKEN": owner_token},
        )
    )
    job_id = parse_job_id(out)
    assert job_id is not None, f"sbatch failed: {out}"
    wait_job_state(c, job_id, "R", timeout=60)
    return job_id


def _assoc_mgr_as(c, user: str) -> str:
    return c.cli_as_user(
        user,
        ["scontrol", "show", "assoc_mgr"],
        extra_env={"SPUR_AUTH_TOKEN": _token_for(c, user)},
    )


class TestUsagePrivate:
    """``private_data = ["usage"]``: ``scontrol show assoc_mgr`` lists only the
    scopes the caller takes part in."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _jwt_auth(private_data=["usage"], cluster_admins=[JOB_OWNER])

    def test_assoc_mgr_hides_scopes_the_caller_has_no_part_in(self, accounting_cluster):
        c = accounting_cluster
        login = _login(c)
        job_id = _owner_job_in_qos(c, "priv-usage-qos")
        try:
            assoc = _assoc_mgr_as(c, login)
            assert "QOS=priv-usage-qos" not in assoc, (
                f"{login} must not see a QOS only {JOB_OWNER} uses:\n{assoc}"
            )
        finally:
            _cancel_as_owner(c, job_id)


class TestUsageVisibleByDefault:
    """With ``private_data`` unset, every caller sees every assoc_mgr scope."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _jwt_auth(cluster_admins=[JOB_OWNER])

    def test_assoc_mgr_lists_other_users_scopes(self, accounting_cluster):
        c = accounting_cluster
        login = _login(c)
        job_id = _owner_job_in_qos(c, "vis-usage-qos")
        try:
            assoc = _assoc_mgr_as(c, login)
            assert "QOS=vis-usage-qos" in assoc, (
                f"{login} must see {JOB_OWNER}'s QOS by default:\n{assoc}"
            )
        finally:
            _cancel_as_owner(c, job_id)
