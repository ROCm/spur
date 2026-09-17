# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""The controller reconciles what a node is holding against its own log, on an
operator's word."""

import pytest

from cluster import job_state, wait_until
from test_admission_ledger import (
    read_run_record,
    run_supervisors,
    submit_holder,
    wait_run_record,
)

JWT_KEY = "e2e-reconcile-key"


def ledger_pulls(cluster, node_index: int = 0) -> int:
    return cluster.spurd_log(node_index).count("answered a ledger pull")


def reconcile(cluster, node_name: str, extra_env: dict[str, str] | None = None):
    return cluster.cli_with_exit(
        ["scontrol", "update", f"NodeName={node_name}", "Reconcile=yes"],
        extra_env=extra_env,
    )


def start_authenticated(cluster):
    cluster.start(config_overrides={"auth": {"plugin": "jwt", "jwt_key": JWT_KEY}})
    return cluster


def mint_token(cluster, user: str, admin: bool = False) -> str:
    args = ["spur", "token", "user", "--user", user]
    if admin:
        args.append("--admin")
    args += ["--config", f"{cluster.etc_dir}/spur.conf"]
    token = cluster.cli(args).strip().splitlines()[-1].strip()
    assert token.count(".") == 2, f"unexpected token format: {token!r}"
    return token


class TestOperatorReconcile:
    def test_reconciling_a_healthy_node_pulls_its_ledger_and_cancels_nothing(
        self, cluster
    ):
        node = cluster.node_names[0]
        job_ids = [
            submit_holder(cluster, f"rec-keep-{i}", cpus=1, seconds=30)
            for i in range(2)
        ]
        for job_id in job_ids:
            wait_run_record(cluster, job_id)
        before_alloc = cluster.node_cpu_alloc(node)
        assert before_alloc == 2
        before_pulls = ledger_pulls(cluster)
        before_supervisors = {j: run_supervisors(cluster, j) for j in job_ids}

        code, out = reconcile(cluster, node)
        assert code == 0, f"an admin reconcile must succeed:\n{out}"

        wait_until(
            lambda: ledger_pulls(cluster) > before_pulls,
            f"node {node} never answered a ledger pull after the reconcile",
        )
        # Without this a reconciler that cancels everything looks like one that
        # works: only the surviving jobs tell the two apart.
        for job_id in job_ids:
            assert job_state(cluster.squeue_all(), job_id) == "R", (
                f"reconciling a healthy node ended job {job_id}:\n"
                f"{cluster.squeue_all()}"
            )
            record = read_run_record(cluster, job_id)
            assert record is not None and record["slice_released"] is False, record
        assert cluster.node_cpu_alloc(node) == before_alloc
        for job_id, pids in before_supervisors.items():
            assert pids <= run_supervisors(cluster, job_id)

        for job_id in job_ids:
            cluster.scancel(str(job_id))

    # The authz bar must hold on a cluster that authenticates nobody, without
    # locking its operators out — see the root case in the tests above.
    def test_a_non_admin_is_refused_on_a_cluster_with_no_authentication(self, cluster):
        submit_user = cluster.nodes[0].user
        if submit_user == "root":
            pytest.skip("need a non-root SSH user to test non-admin rejection")
        probe = cluster.cli_as_user("root", ["scontrol", "show", "config"])
        if "sudo" in probe.lower() and (
            "password" in probe.lower() or "not allowed" in probe.lower()
        ):
            pytest.skip(f"sudo -u unavailable in this environment: {probe.strip()}")

        node = cluster.node_names[0]
        before = ledger_pulls(cluster)
        out = cluster.cli_as_user(
            submit_user,
            ["scontrol", "update", f"NodeName={node}", "Reconcile=yes"],
        )
        assert "requires cluster admin" in out.lower(), (
            f"a non-admin reconcile must be denied even with auth off: {out}"
        )
        assert ledger_pulls(cluster) == before, (
            "a refused reconcile must not reach the node"
        )

    def test_only_an_admin_may_ask_a_node_to_reconcile(self, unstarted_cluster):
        cluster = start_authenticated(unstarted_cluster)
        node = cluster.node_names[0]

        before_pulls = ledger_pulls(cluster)
        code, out = reconcile(
            cluster, node,
            extra_env={"SPUR_AUTH_TOKEN": mint_token(cluster, "e2e-nonadmin")},
        )
        assert code != 0, f"a non-admin reconcile must be rejected:\n{out}"
        assert ledger_pulls(cluster) == before_pulls, (
            "the rejected reconcile still pulled the node's ledger"
        )

        code, out = reconcile(
            cluster, node,
            extra_env={"SPUR_AUTH_TOKEN": mint_token(cluster, "e2e-admin", admin=True)},
        )
        assert code == 0, f"an admin reconcile must succeed:\n{out}"
        wait_until(
            lambda: ledger_pulls(cluster) > before_pulls,
            f"node {node} never answered the admin's ledger pull",
        )


class TestUnrecordedClaim:
    def test_a_claim_the_controller_has_no_record_of_is_ended_and_its_cores_freed(
        self, cluster
    ):
        node = cluster.node_names[0]
        job_id = submit_holder(cluster, "rec-orphan", cpus=3, seconds=120)
        wait_run_record(cluster, job_id)
        assert cluster.node_cpu_alloc(node) == 3
        assert run_supervisors(cluster, job_id), "a running job must have a supervisor"

        cluster.restart_controller(forget_history=True)
        assert cluster.node_cpu_alloc(node) == 0, (
            "a controller that forgot its log must not be accounting for this run"
        )
        record = read_run_record(cluster, job_id)
        assert record is not None and record["slice_released"] is False, (
            f"the node stopped holding the run before any reconcile: {record}"
        )

        code, out = reconcile(cluster, node)
        assert code == 0, f"an admin reconcile must succeed:\n{out}"

        wait_until(
            lambda: not run_supervisors(cluster, job_id),
            f"job {job_id} kept running after the reconcile that disowned it",
        )
        # A swept record is a released one: it leaves the spool only once the
        # slice has gone back.
        wait_until(
            lambda: (read_run_record(cluster, job_id) or {"slice_released": True})[
                "slice_released"
            ],
            f"job {job_id}'s cores were never handed back after the reconcile",
        )
        assert cluster.node_cpu_alloc(node) == 0
