# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for node reservations."""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


class TestReservations:
    def test_create_list_and_delete_reservation(self, cluster):
        res_name = f"res-e2e-{int(time.time())}"
        node = cluster.node_names[0]
        create_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={node}",
                "--users=testuser",
            ],
        )
        assert "created" in create_out.lower()

        show_out = cluster.scontrol("show", "reservation")
        assert res_name in show_out
        assert node in show_out
        assert "ACTIVE" in show_out or "INACTIVE" in show_out

        # A distinct node avoids overlap; fall back to the overlap flag when the cluster has one node.
        other_name = f"res-e2e-other-{int(time.time())}"
        if len(cluster.node_names) >= 2:
            other_node = cluster.node_names[1]
            overlap_flags = []
        else:
            other_node = node
            overlap_flags = ["--flags=overlap"]
        other_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={other_name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={other_node}",
                *overlap_flags,
            ],
        )
        assert "created" in other_out.lower()

        filtered = cluster.scontrol("show", "reservation", res_name)
        assert res_name in filtered
        assert other_name not in filtered

        # An unknown reservation name is an error, not an empty success.
        code, out = cluster.cli_with_exit(
            ["scontrol", "show", "reservation", "no-such-reservation"]
        )
        assert code != 0
        assert "not found" in out.lower()

        cluster.cli_as_user("root", ["scontrol", "delete-reservation", other_name])

        delete_out = cluster.cli_as_user(
            "root", ["scontrol", "delete-reservation", res_name]
        )
        assert "deleted" in delete_out.lower()

        show_after = cluster.scontrol("show", "reservation")
        assert res_name not in show_after

    def test_unauthorized_job_blocked_on_reserved_node(self, cluster):
        res_name = f"res-block-{int(time.time())}"
        node = cluster.node_names[0]
        create_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=30",
                f"--nodes={node}",
                "--users=resuser",
            ],
        )
        assert "created" in create_out.lower()

        script = cluster.write_file("res-block.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-N", "1", "-w", node, "-t", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job_state(cluster, job_id, "PD", timeout=30)

    def test_reservation_job_schedules_for_authorized_user(self, cluster):
        res_name = f"res-auth-{int(time.time())}"
        node = cluster.node_names[0]
        submit_user = cluster.nodes[0].user
        create_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=30",
                f"--nodes={node}",
                f"--users={submit_user}",
            ],
        )
        assert "created" in create_out.lower()

        script = cluster.write_file("res-auth.sh", "#!/bin/bash\necho RES_OK\n")
        out_path = f"{cluster.remote_dir}/res-auth.out"
        sb = cluster.sbatch(
            [
                "-N",
                "1",
                f"--reservation={res_name}",
                "-w",
                node,
                "-t",
                "1",
                "-o",
                out_path,
                script,
            ]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "RES_OK" in content

    def test_hold_on_delete_and_release(self, cluster):
        res_name = f"res-hold-{int(time.time())}"
        node = cluster.node_names[0]
        create_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={node}",
                "--users=testuser",
            ],
        )
        assert "created" in create_out.lower()

        script = cluster.write_file("res-hold.sh", "#!/bin/bash\necho HOLD_RELEASE_OK\n")
        out_path = f"{cluster.remote_dir}/res-hold.out"
        sb = cluster.sbatch(
            [
                "-N",
                "1",
                f"--reservation={res_name}",
                "-w",
                node,
                "-t",
                "1",
                "-o",
                out_path,
                script,
            ]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job_state(cluster, job_id, "PD", timeout=30)

        cluster.cli_as_user("root", ["scontrol", "delete-reservation", res_name])

        wait_job_state(cluster, job_id, "PD", timeout=30)
        held = cluster.squeue(["-j", str(job_id), "-o", "%t %r"])
        assert "PD" in held
        assert "ReservationDeleted" in held

        cluster.scontrol("release", str(job_id))

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed after release, got {state}"
        content = cluster.read_output_on_any_node(out_path)
        assert "HOLD_RELEASE_OK" in content

    def test_no_hold_jobs_delete(self, cluster):
        res_name = f"res-nohold-{int(time.time())}"
        node = cluster.node_names[0]
        create_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={node}",
                "--users=testuser",
                "--flags=no_hold_jobs",
            ],
        )
        assert "created" in create_out.lower()

        script = cluster.write_file("res-nohold.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(
            [
                "-N",
                "1",
                f"--reservation={res_name}",
                "-w",
                node,
                "-t",
                "1",
                script,
            ]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job_state(cluster, job_id, "PD", timeout=30)

        cluster.cli_as_user("root", ["scontrol", "delete-reservation", res_name])

        wait_job_state(cluster, job_id, "PD", timeout=30)
        show = cluster.squeue(["-j", str(job_id), "-o", "%t %r %v"])
        assert "PD" in show
        assert "Held" not in show
        assert res_name not in show

    def test_create_rejects_busy_node_without_ignore_jobs(self, cluster):
        node = cluster.node_names[0]
        long_script = cluster.write_file("res-long.sh", "#!/bin/bash\nsleep 300\n")
        sb = cluster.sbatch(["-N", "1", "-w", node, "-t", "10", long_script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job_state(cluster, job_id, "R", timeout=30)

        res_name = f"res-busy-{int(time.time())}"
        out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=10",
                f"--nodes={node}",
            ],
        )
        msg = out.lower()
        assert "busy" in msg or "until after reservation start" in msg, f"unexpected: {out}"

    def test_reservation_management_requires_privileged_user(self, cluster):
        """Reservation create/update/delete via the CLI is restricted to root or
        sudo/wheel members. Uses the always-present unprivileged 'nobody'
        account, so no user provisioning is needed."""
        # Probe as `nobody` (show is unguarded): confirms sudo -u and binary exec
        # work for that account; skip rather than fail if not.
        probe = cluster.cli_as_user("nobody", ["scontrol", "show", "reservation"])
        low = probe.lower()
        if (
            "sudo" in low and ("password" in low or "not allowed" in low)
        ) or "permission denied" in low:
            pytest.skip(f"cannot run CLI as 'nobody' in this environment: {probe.strip()}")

        res_name = f"res-priv-{int(time.time())}"
        node = cluster.node_names[0]

        denied_msg = "requires root or membership"

        # The read path must stay open to everyone (not accidentally gated).
        assert denied_msg not in low, f"show reservation should be readable by all: {probe}"

        # Unprivileged create is denied (explicit subcommand).
        create_denied = cluster.cli_as_user(
            "nobody",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={node}",
            ],
        )
        assert denied_msg in create_denied.lower(), f"unexpected: {create_denied}"
        assert res_name not in cluster.scontrol("show", "reservation")

        # Unprivileged create is denied via the Slurm-inline syntax too.
        inline_denied = cluster.cli_as_user(
            "nobody",
            [
                "scontrol",
                "create",
                f"ReservationName={res_name}",
                "StartTime=now",
                "Duration=60",
                f"Nodes={node}",
            ],
        )
        assert denied_msg in inline_denied.lower(), f"unexpected: {inline_denied}"
        assert res_name not in cluster.scontrol("show", "reservation")

        try:
            # Privileged (root) create succeeds.
            create_ok = cluster.cli_as_user(
                "root",
                [
                    "scontrol",
                    "create-reservation",
                    f"--name={res_name}",
                    "--start-time=now",
                    "--duration=60",
                    f"--nodes={node}",
                ],
            )
            assert "created" in create_ok.lower(), f"create failed: {create_ok}"
            assert res_name in cluster.scontrol("show", "reservation")

            # Unprivileged update and delete are denied.
            upd_denied = cluster.cli_as_user(
                "nobody",
                ["scontrol", "update-reservation", f"--name={res_name}", "--duration=120"],
            )
            assert denied_msg in upd_denied.lower(), f"unexpected: {upd_denied}"

            del_denied = cluster.cli_as_user(
                "nobody", ["scontrol", "delete-reservation", res_name]
            )
            assert denied_msg in del_denied.lower(), f"unexpected: {del_denied}"
            assert res_name in cluster.scontrol("show", "reservation")

            # Unprivileged delete via the Slurm-inline syntax is denied too.
            inline_del_denied = cluster.cli_as_user(
                "nobody", ["scontrol", "delete", f"ReservationName={res_name}"]
            )
            assert denied_msg in inline_del_denied.lower(), f"unexpected: {inline_del_denied}"
            assert res_name in cluster.scontrol("show", "reservation")

            # Privileged (root) delete succeeds.
            del_ok = cluster.cli_as_user(
                "root", ["scontrol", "delete-reservation", res_name]
            )
            assert "deleted" in del_ok.lower(), f"privileged delete failed: {del_ok}"
            assert res_name not in cluster.scontrol("show", "reservation")
        finally:
            # A leaked reservation fences a node for an hour; best-effort cleanup.
            cluster.cli_as_user("root", ["scontrol", "delete-reservation", res_name])

    def test_non_owner_cannot_delete_or_update_reservation(self, cluster):
        """A reservation is owned by its creator; a different user must not be
        able to delete or update it, but the owner still can. Exercises the
        full CLI -> gRPC -> controller ownership check (SPUR-69)."""
        submit_user = cluster.nodes[0].user
        if submit_user == "root":
            pytest.skip("need a non-root SSH user to test non-owner rejection")

        # Verify passwordless/known-password sudo -u works in this environment;
        # otherwise we cannot assume a second identity.
        probe = cluster.cli_as_user("root", ["scontrol", "show", "reservation"])
        if "sudo" in probe.lower() and (
            "password" in probe.lower() or "not allowed" in probe.lower()
        ):
            pytest.skip(f"sudo -u unavailable in this environment: {probe.strip()}")

        # A non-owner is denied by the server ownership check or, if unprivileged,
        # by the client gate first. Accept both so this never silently skips.
        denied = ("cannot delete", "cannot modify", "permission", "requires root or membership")

        res_name = f"res-owner-{int(time.time())}"
        node = cluster.node_names[0]

        # Create as root -> owner is root.
        create_out = cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={node}",
                "--users=testuser",
            ],
        )
        assert "created" in create_out.lower(), f"create failed: {create_out}"

        show_out = cluster.scontrol("show", "reservation")
        assert res_name in show_out
        assert "Owner=root" in show_out

        # Non-owner (the ordinary SSH user) delete must be rejected.
        del_denied = cluster.cli_as_user(
            submit_user, ["scontrol", "delete-reservation", res_name]
        )
        assert "deleted" not in del_denied.lower(), f"unexpected delete: {del_denied}"
        assert any(m in del_denied.lower() for m in denied), f"unexpected: {del_denied}"
        assert res_name in cluster.scontrol("show", "reservation")

        # Non-owner update must be rejected too.
        upd_denied = cluster.cli_as_user(
            submit_user,
            ["scontrol", "update-reservation", f"--name={res_name}", "--duration=120"],
        )
        assert any(m in upd_denied.lower() for m in denied), f"unexpected: {upd_denied}"
        assert res_name in cluster.scontrol("show", "reservation")

        # Owner (root) can still delete.
        del_ok = cluster.cli_as_user(
            "root", ["scontrol", "delete-reservation", res_name]
        )
        assert "deleted" in del_ok.lower(), f"owner delete failed: {del_ok}"
        assert res_name not in cluster.scontrol("show", "reservation")
