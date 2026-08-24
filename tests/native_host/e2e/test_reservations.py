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

    def test_reservation_duration_formats(self, cluster):
        """Duration accepts whole minutes and Slurm time (HH:MM:SS, D-HH:MM:SS)
        via both the subcommand and inline syntax. A positive duration must
        survive the scheduler's expired-reservation purge; an unparseable, zero,
        or omitted duration is rejected up front, not silently created then
        purged (SPUR-182)."""
        node = cluster.node_names[0]

        def create(name, kind, duration):
            if kind == "subcommand":
                args = [
                    "scontrol",
                    "create-reservation",
                    f"--name={name}",
                    "--start-time=now",
                    f"--duration={duration}",
                    f"--nodes={node}",
                    "--flags=ignore_jobs",
                ]
            else:
                args = [
                    "scontrol",
                    "create",
                    f"ReservationName={name}",
                    "StartTime=now",
                    f"Duration={duration}",
                    f"Nodes={node}",
                    "Flags=IGNORE_JOBS",
                ]
            return cluster.cli_as_user("root", args)

        def classify(name):
            # Exit code alone can't tell a definitive "not found" from a
            # transient error (both non-zero), so match the message; that lets
            # callers retry hiccups instead of misreading them as a purge.
            out = cluster.cli_allow_fail(["scontrol", "show", "reservation", name])
            if f"ReservationName={name}" in out:
                return "present"
            if f"Reservation {name} not found" in out:
                return "absent"
            return "error"

        def assert_survives_purge(name, window=5.0, step=0.5):
            # Must stay listed across a purge cycle (~1s): fail on a definitive
            # "not found", tolerate transient errors, and require one real
            # sighting so a run of errors can't pass vacuously.
            deadline = time.time() + window
            seen = False
            while time.time() < deadline:
                state = classify(name)
                if state == "present":
                    seen = True
                elif state == "absent":
                    raise AssertionError(f"reservation {name} was purged unexpectedly")
                time.sleep(step)
            assert seen, f"reservation {name} was never observed present within {window}s"

        def assert_never_created(name, window=3.0, step=0.5):
            # A rejected create must leave nothing: require a definitive "not
            # found", retrying past transient errors; fail if it ever appears.
            deadline = time.time() + window
            confirmed_absent = False
            while time.time() < deadline:
                state = classify(name)
                if state == "present":
                    raise AssertionError(f"reservation {name} must not have been created")
                if state == "absent":
                    confirmed_absent = True
                    break
                time.sleep(step)
            assert confirmed_absent, f"could not confirm reservation {name} absent within {window}s"

        # Positive durations across both entry points and all accepted formats.
        # 30-00:00:00 is the exact reported case (30 days).
        valid_cases = [
            ("subcommand", "30-00:00:00"),
            ("inline", "30-00:00:00"),
            ("inline", "01:00:00"),
            ("subcommand", "60"),
        ]
        for idx, (kind, duration) in enumerate(valid_cases):
            name = f"res-dur-{idx}-{int(time.time())}"
            try:
                out = create(name, kind, duration)
                assert "created" in out.lower(), f"{kind} {duration} create failed: {out}"
                assert_survives_purge(name)
            finally:
                cluster.cli_as_user("root", ["scontrol", "delete-reservation", name])

        # Unique names keep an unexpectedly-successful case isolated (no
        # cascading "already exists") and cleaned up on its own.
        rejection_cases = [
            ("subcommand", "notatime"),
            ("inline", "notatime"),
            ("subcommand", "0"),
            ("inline", "0"),
        ]
        for idx, (kind, duration) in enumerate(rejection_cases):
            name = f"res-dur-rej-{idx}-{int(time.time())}"
            try:
                out = create(name, kind, duration)
                assert "created" not in out.lower(), f"{kind} {duration} should be rejected: {out}"
                assert_never_created(name)
            finally:
                # Defensive cleanup if a bug let the create through; a leaked
                # reservation fences a node.
                cluster.cli_as_user("root", ["scontrol", "delete-reservation", name])

        # Omitting Duration in the inline path is rejected too.
        omit_name = f"res-dur-omit-{int(time.time())}"
        try:
            omit_out = cluster.cli_as_user(
                "root",
                [
                    "scontrol",
                    "create",
                    f"ReservationName={omit_name}",
                    "StartTime=now",
                    f"Nodes={node}",
                ],
            )
            assert "created" not in omit_out.lower(), (
                f"omitted duration should be rejected: {omit_out}"
            )
            assert_never_created(omit_name)
        finally:
            cluster.cli_as_user("root", ["scontrol", "delete-reservation", omit_name])

    def test_reservation_management_is_privilege_not_ownership(self, cluster):
        """Managing a reservation takes operator privilege, not ownership: an
        operator may update or delete a reservation somebody else created, while
        an unprivileged user is refused even for one it could see. Exercises the
        full CLI -> gRPC -> controller path."""
        submit_user = cluster.nodes[0].user
        if submit_user == "root":
            pytest.skip("need a non-root SSH user to test a non-owner operator")

        # Verify passwordless/known-password sudo -u works in this environment;
        # otherwise we cannot assume a second identity.
        probe = cluster.cli_as_user("root", ["scontrol", "show", "reservation"])
        if "sudo" in probe.lower() and (
            "password" in probe.lower() or "not allowed" in probe.lower()
        ):
            pytest.skip(f"sudo -u unavailable in this environment: {probe.strip()}")

        groups = cluster.nodes[0].exec_allow_fail(f"id -nG '{submit_user}'").split()
        submit_user_is_operator = bool({"sudo", "wheel"} & set(groups))

        # Denial can come from the client pre-check or the controller's gate; both
        # are legitimate, so accept either wording.
        denied = ("may not manage", "permission", "requires root or membership")

        res_name = f"res-owner-{int(time.time())}"
        node = cluster.node_names[0]

        try:
            # Create as root -> owner is root, so every later caller is a non-owner.
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

            # An unprivileged non-owner is refused both operations.
            for args, verb in (
                (["delete-reservation", res_name], "deleted"),
                (
                    ["update-reservation", f"--name={res_name}", "--duration=120"],
                    "updated",
                ),
            ):
                out = cluster.cli_as_user("nobody", ["scontrol"] + args)
                assert verb not in out.lower(), f"unprivileged {verb}: {out}"
                assert any(m in out.lower() for m in denied), f"unexpected: {out}"
                assert res_name in cluster.scontrol("show", "reservation")

            # The SSH user is a non-owner either way; whether it may manage the
            # reservation follows from its privilege, which is the whole point.
            upd_out = cluster.cli_as_user(
                submit_user,
                [
                    "scontrol",
                    "update-reservation",
                    f"--name={res_name}",
                    "--duration=120",
                ],
            )
            if submit_user_is_operator:
                assert "updated" in upd_out.lower(), (
                    f"an operator must be able to update another user's "
                    f"reservation: {upd_out}"
                )
                del_out = cluster.cli_as_user(
                    submit_user, ["scontrol", "delete-reservation", res_name]
                )
                assert "deleted" in del_out.lower(), (
                    f"an operator must be able to delete another user's "
                    f"reservation: {del_out}"
                )
                assert res_name not in cluster.scontrol("show", "reservation")
            else:
                assert any(m in upd_out.lower() for m in denied), (
                    f"unexpected: {upd_out}"
                )
                assert res_name in cluster.scontrol("show", "reservation")
        finally:
            # A leaked reservation fences a node for an hour; best-effort cleanup.
            cluster.cli_as_user("root", ["scontrol", "delete-reservation", res_name])


class TestReservationServerGate:
    """The controller's operator rule, proven independently of the CLI's own check.

    Every other path here is refused client-side before a request is sent, so none
    of them show the controller enforcing anything. A credential splits the two:
    the CLI runs as root and passes locally, while the gate judges the token's
    identity."""

    AUTH_CONFIG = {"auth": {"plugin": "jwt", "jwt_key": "e2e-reservation-key"}}

    @pytest.fixture
    def cluster_config_overrides(self):
        return self.AUTH_CONFIG

    def _token_for(self, cluster, user: str) -> str:
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

    def _create_as(self, cluster, token: str, name: str) -> str:
        return cluster.cli_as_user(
            "root",
            [
                "scontrol",
                "create-reservation",
                f"--name={name}",
                "--start-time=now",
                "--duration=60",
                f"--nodes={cluster.node_names[0]}",
                "--flags=ignore_jobs",
            ],
            extra_env={"SPUR_AUTH_TOKEN": token},
        )

    def test_credential_the_controller_cannot_resolve_is_denied(self, cluster):
        """A verified non-admin whose name resolves nowhere is refused, even though
        the CLI ran as root. Fails closed: an unresolvable caller is not an allow."""
        name = f"res-gate-{int(time.time())}"
        try:
            out = self._create_as(
                cluster, self._token_for(cluster, "no_such_user_in_nss_7f3a"), name
            ).lower()

            # The CLI's own refusal would prove nothing, so require the server's wording.
            assert "insufficient privileges" not in out, (
                f"the client pre-check fired, so the controller was never asked: {out}"
            )
            assert "cannot verify" in out and "may manage reservations" in out, (
                f"expected the controller's unresolvable-caller denial: {out}"
            )
            assert name not in cluster.scontrol("show", "reservation")
        finally:
            cluster.cli_as_user("root", ["scontrol", "delete-reservation", name])

    def test_operator_credential_is_allowed(self, cluster):
        """The counterpart, so the denial above cannot be explained by credentials
        being broken: the same path with a token for root creates the reservation."""
        name = f"res-gate-ok-{int(time.time())}"
        try:
            out = self._create_as(cluster, self._token_for(cluster, "root"), name)
            assert "created" in out.lower(), f"an operator credential must be accepted: {out}"
            assert name in cluster.scontrol("show", "reservation")
        finally:
            cluster.cli_as_user("root", ["scontrol", "delete-reservation", name])
