# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for node reservations."""

import time

from cluster import parse_job_id, wait_job, wait_job_state


class TestReservations:
    def test_create_list_and_delete_reservation(self, cluster):
        res_name = f"res-e2e-{int(time.time())}"
        node = cluster.node_names[0]
        create_out = cluster.scontrol(
            "create-reservation",
            f"--name={res_name}",
            "--start-time=now",
            "--duration=60",
            f"--nodes={node}",
            "--users=testuser",
        )
        assert "created" in create_out.lower()

        show_out = cluster.scontrol("show", "reservation")
        assert res_name in show_out
        assert node in show_out
        assert "ACTIVE" in show_out or "INACTIVE" in show_out

        delete_out = cluster.scontrol("delete-reservation", res_name)
        assert "deleted" in delete_out.lower()

        show_after = cluster.scontrol("show", "reservation")
        assert res_name not in show_after

    def test_unauthorized_job_blocked_on_reserved_node(self, cluster):
        res_name = f"res-block-{int(time.time())}"
        node = cluster.node_names[0]
        cluster.scontrol(
            "create-reservation",
            f"--name={res_name}",
            "--start-time=now",
            "--duration=30",
            f"--nodes={node}",
            "--users=resuser",
        )

        script = cluster.write_file("res-block.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-N", "1", "-w", node, "-t", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job_state(cluster, job_id, "PD", timeout=30)

    def test_reservation_job_schedules_for_authorized_user(self, cluster):
        res_name = f"res-auth-{int(time.time())}"
        node = cluster.node_names[0]
        cluster.scontrol(
            "create-reservation",
            f"--name={res_name}",
            "--start-time=now",
            "--duration=30",
            f"--nodes={node}",
            "--users=testuser",
        )

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
        cluster.scontrol(
            "create-reservation",
            f"--name={res_name}",
            "--start-time=now",
            "--duration=60",
            f"--nodes={node}",
            "--users=testuser",
        )

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

        cluster.scontrol("delete-reservation", res_name)

        wait_job_state(cluster, job_id, "PD", timeout=30)
        held = cluster.squeue(["-j", str(job_id), "-o", "%T %r"])
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
        cluster.scontrol(
            "create-reservation",
            f"--name={res_name}",
            "--start-time=now",
            "--duration=60",
            f"--nodes={node}",
            "--users=testuser",
            "--flags=no_hold_jobs",
        )

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

        cluster.scontrol("delete-reservation", res_name)

        wait_job_state(cluster, job_id, "PD", timeout=30)
        show = cluster.squeue(["-j", str(job_id), "-o", "%T %r %v"])
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
        out = cluster.cli_allow_fail(
            [
                "scontrol",
                "create-reservation",
                f"--name={res_name}",
                "--start-time=now",
                "--duration=10",
                f"--nodes={node}",
            ]
        )
        msg = out.lower()
        assert "busy" in msg or "until after reservation start" in msg, f"unexpected: {out}"
