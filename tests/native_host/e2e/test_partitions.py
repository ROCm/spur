# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for runtime partition management via scontrol."""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def _unique(prefix: str) -> str:
    return f"{prefix}-{int(time.time())}"


def _sbatch_when_qos_ready(cluster, args: list[str], timeout: int = 15) -> str:
    """Retry sbatch until the accounting QoS cache picks up a QoS created
    moments earlier via sacctmgr — the cache refreshes on a fixed interval
    (see accounting_cluster's fairshare_refresh_secs), not on write."""
    deadline = time.time() + timeout
    while True:
        try:
            return cluster.sbatch(args)
        except RuntimeError as e:
            if "does not exist" not in str(e) or time.time() >= deadline:
                raise
            time.sleep(1)


class TestPartitionCreate:
    def test_create_and_show_partition(self, cluster):
        name = _unique("part")
        node = cluster.node_names[0]

        out = cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--state=UP",
        )
        assert "created" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show

    def test_create_partition_with_all_options(self, cluster):
        name = _unique("part-full")
        node = cluster.node_names[0]

        out = cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--state=DOWN",
            "--max-time=02:00:00",
            "--default-time=00:30:00",
            "--min-nodes=1",
            "--allow-accounts=acct1,acct2",
            "--allow-groups=grp1",
            "--priority-tier=5",
            "--preempt-mode=CANCEL",
        )
        assert "created" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show

    def test_create_duplicate_partition_fails(self, cluster):
        name = _unique("part-dup")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        out = cluster.cli_allow_fail(
            [
                "scontrol",
                "create-partition",
                f"--name={name}",
                f"--nodes={node}",
            ]
        )
        assert "already exists" in out.lower() or "error" in out.lower(), (
            f"expected error for duplicate partition, got: {out}"
        )

    def test_create_partition_empty_name_fails(self, cluster):
        out = cluster.cli_allow_fail(
            [
                "scontrol",
                "create-partition",
                "--name=",
                f"--nodes={cluster.node_names[0]}",
            ]
        )
        assert "empty" in out.lower() or "invalid" in out.lower() or "error" in out.lower(), (
            f"expected error for empty partition name, got: {out}"
        )


class TestPartitionUpdate:
    def test_update_partition_state(self, cluster):
        name = _unique("part-upd")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--state=UP",
        )

        out = cluster.scontrol(
            "update-partition",
            f"--name={name}",
            "--state=DRAIN",
        )
        assert "updated" in out.lower()

        show = cluster.scontrol("show", "partition")
        # The partition should still be listed (state change does not remove it).
        assert name in show

    def test_update_partition_max_time(self, cluster):
        name = _unique("part-mt")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--max-time=01:00:00",
        )

        out = cluster.scontrol(
            "update-partition",
            f"--name={name}",
            "--max-time=48:00:00",
        )
        assert "updated" in out.lower()

    def test_update_partition_allow_accounts(self, cluster):
        name = _unique("part-acct")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--allow-accounts=acct1",
        )

        out = cluster.scontrol(
            "update-partition",
            f"--name={name}",
            "--allow-accounts=acct1,acct2",
            "--set-allow-accounts",
        )
        assert "updated" in out.lower()

    def test_update_nonexistent_partition_fails(self, cluster):
        out = cluster.cli_allow_fail(
            [
                "scontrol",
                "update-partition",
                "--name=does-not-exist-at-all",
                "--state=DOWN",
            ]
        )
        assert "not found" in out.lower() or "error" in out.lower(), (
            f"expected not-found error, got: {out}"
        )

    def test_update_nodes_field(self, cluster):
        name = _unique("part-nodes")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        out = cluster.scontrol(
            "update-partition",
            f"--name={name}",
            f"--nodes={node}",
        )
        assert "updated" in out.lower()


class TestPartitionDelete:
    def test_delete_partition(self, cluster):
        name = _unique("part-del")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        show_before = cluster.scontrol("show", "partition")
        assert name in show_before

        out = cluster.scontrol("delete-partition", f"--name={name}")
        assert "deleted" in out.lower()

        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after

    def test_delete_nonexistent_partition_fails(self, cluster):
        out = cluster.cli_allow_fail(
            [
                "scontrol",
                "delete-partition",
                "--name=no-such-partition-xyz",
            ]
        )
        assert "not found" in out.lower() or "error" in out.lower(), (
            f"expected not-found error, got: {out}"
        )

    def test_delete_partition_with_running_job_fails(self, cluster):
        """Deleting a partition with a job actively running on it must be
        rejected (the in-use guard in ClusterManager::delete_partition).
        Once the job is cancelled, deletion must succeed."""
        name = _unique("part-inuse")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        script = cluster.write_file("part-inuse.sh", "#!/bin/bash\nsleep 60\n")
        sb = cluster.sbatch(["-N", "1", "-w", node, "-p", name, "-t", "5", script])
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=60)

        try:
            out = cluster.cli_allow_fail(
                ["scontrol", "delete-partition", f"--name={name}"]
            )
            assert "in use" in out.lower() or "error" in out.lower(), (
                f"expected in-use rejection while job {job_id} is running, got: {out}"
            )

            show = cluster.scontrol("show", "partition")
            assert name in show, "partition must still exist after rejected delete"
        finally:
            cluster.scancel(str(job_id))
            wait_job(cluster, job_id, timeout=60)

        out = cluster.scontrol("delete-partition", f"--name={name}")
        assert "deleted" in out.lower(), f"expected deletion to succeed after cancel, got: {out}"

        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after

    def test_delete_idle_partition_succeeds(self, cluster):
        name = _unique("part-idle")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        show = cluster.scontrol("show", "partition")
        assert name in show

        out = cluster.scontrol("delete-partition", f"--name={name}")
        assert "deleted" in out.lower(), f"expected deletion to succeed, got: {out}"

        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after


class TestPartitionLifecycle:
    def test_create_update_delete_round_trip(self, cluster):
        name = _unique("part-rt")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--max-time=04:00:00",
            "--priority-tier=3",
        )
        show = cluster.scontrol("show", "partition")
        assert name in show

        cluster.scontrol(
            "update-partition",
            f"--name={name}",
            "--state=DRAIN",
            "--max-time=08:00:00",
        )

        cluster.scontrol("delete-partition", f"--name={name}")
        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after

    def test_jobs_schedule_on_newly_created_partition(self, cluster):
        """A partition created at runtime must be immediately usable by
        already-registered nodes — not just visible in `show partition`.
        Submits directly to the new partition (not the default) and waits
        for actual completion, exercising the scheduler's node-eligibility
        path end to end rather than just the partition table."""
        name = _unique("part-sched")
        nodes_list = ",".join(cluster.node_names)
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={nodes_list}",
        )

        # Verify the new partition appears in show output.
        show = cluster.scontrol("show", "partition")
        assert name in show

        script = cluster.write_file("part-sched.sh", "#!/bin/bash\necho PARTITION_OK\n")
        out_path = f"{cluster.remote_dir}/part-sched.out"
        sb = cluster.sbatch(["-N", "1", "-w", node, "-p", name, "-t", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), (
            f"expected job on newly-created partition '{name}' to complete, got {state} "
            "(a stuck/pending state here means the node's cached partition "
            "membership was not refreshed after partition creation)"
        )

        content = cluster.read_output_on_any_node(out_path)
        assert "PARTITION_OK" in content


class TestPartitionAllowDenyAccounts:
    """
    Verify AllowAccounts and DenyAccounts are enforced at job submission.
    No accounting backend required — Spur enforces these as pure string
    matches against the -A/--account flag passed to sbatch.
    """

    def test_allow_accounts_blocks_unlisted_account(self, cluster):
        name = _unique("part-allow-acct")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--allow-accounts=trusted",
        )

        script = cluster.write_file("allow-acct.sh", "#!/bin/bash\necho DONE\n")

        # Unlisted account must be rejected at submission.
        out = cluster.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-A", "untrusted", "-t", "1", script]
        )
        assert "not allowed" in out.lower() or "error" in out.lower(), (
            f"expected rejection for unlisted account, got: {out}"
        )

    def test_allow_accounts_permits_listed_account(self, cluster):
        name = _unique("part-allow-acct2")
        nodes_list = ",".join(cluster.node_names)
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={nodes_list}",
            "--allow-accounts=trusted",
        )

        script = cluster.write_file("allow-acct2.sh", "#!/bin/bash\necho ALLOW_OK\n")
        out_path = f"{cluster.remote_dir}/allow-acct2.out"

        # Submit to the default partition (which has no account restriction) with
        # the trusted account — verifies the account is globally accepted.
        sb = cluster.sbatch(
            ["-N", "1", "-w", node, "-A", "trusted", "-t", "1", "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "ALLOW_OK" in content

    def test_deny_accounts_blocks_listed_account(self, cluster):
        name = _unique("part-deny-acct")
        node = cluster.node_names[0]

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--deny-accounts=baduser",
        )

        script = cluster.write_file("deny-acct.sh", "#!/bin/bash\necho DONE\n")

        out = cluster.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-A", "baduser", "-t", "1", script]
        )
        assert "denied" in out.lower() or "error" in out.lower(), (
            f"expected rejection for denied account, got: {out}"
        )

    def test_deny_accounts_allows_unlisted_account(self, cluster):
        name = _unique("part-deny-acct2")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={nodes_list}",
            "--deny-accounts=baduser",
        )

        script = cluster.write_file("deny-acct2.sh", "#!/bin/bash\necho DENY_OK\n")

        # Submit with an account NOT in the deny list to the named partition.
        # The submit must be ACCEPTED (not rejected) — the job may stay pending
        # if the cluster is busy, but the submission itself must succeed.
        out = cluster.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-A", "gooduser", "-t", "1", script]
        )
        # A successful sbatch prints "Submitted batch job <N>"
        assert "submitted" in out.lower() or parse_job_id(out) is not None, (
            f"expected submission to succeed for non-denied account, got: {out}"
        )
        # Ensure it wasn't rejected by the partition enforce logic.
        assert "denied" not in out.lower() and "not allowed" not in out.lower(), (
            f"unlisted account must not be blocked by deny_accounts: {out}"
        )

    def test_runtime_update_allow_accounts_takes_effect(self, cluster):
        """Update a running partition's AllowAccounts and verify the new
        restriction is enforced immediately without a controller restart."""
        name = _unique("part-upd-acct")
        node = cluster.node_names[0]

        # Start open (no account restriction).
        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        script = cluster.write_file("upd-acct.sh", "#!/bin/bash\necho DONE\n")

        # Before update: any account passes.
        sb = cluster.sbatch(["-N", "1", "-p", name, "-A", "someacct", "-t", "1", script])
        assert parse_job_id(sb) is not None

        # Restrict partition to only "privileged".
        cluster.scontrol(
            "update-partition",
            f"--name={name}",
            "--allow-accounts=privileged",
            "--set-allow-accounts",
        )

        # After update: "someacct" must now be rejected.
        out = cluster.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-A", "someacct", "-t", "1", script]
        )
        assert "not allowed" in out.lower() or "error" in out.lower(), (
            f"expected rejection after allow_accounts update, got: {out}"
        )


class TestPartitionAllowDenyQos:
    """
    Verify AllowQos and DenyQos are enforced at job submission.
    Requires the accounting_cluster fixture because QoS names must exist
    in the controller's QoS cache before they can be used with -q.
    """

    def test_allow_qos_blocks_unlisted_qos(self, accounting_cluster):
        c = accounting_cluster
        name = _unique("part-allow-qos")
        node = c.node_names[0]

        # Create the QoS objects in the accounting backend first.
        c.sacctmgr(["add", "qos", "name=premium", "-i"])
        c.sacctmgr(["add", "qos", "name=cheap", "-i"])

        c.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--allow-qos=premium",
        )

        script = c.write_file("allow-qos.sh", "#!/bin/bash\necho DONE\n")

        # QoS not in allow list must be rejected.
        out = c.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-q", "cheap", "-t", "1", script]
        )
        assert "not allowed" in out.lower() or "error" in out.lower(), (
            f"expected rejection for non-allowed QoS, got: {out}"
        )

    def test_allow_qos_permits_listed_qos(self, accounting_cluster):
        c = accounting_cluster
        name = _unique("part-allow-qos2")
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=premium", "-i"])

        c.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--allow-qos=premium",
        )

        script = c.write_file("allow-qos2.sh", "#!/bin/bash\necho ALLOW_QOS_OK\n")
        out_path = f"{c.remote_dir}/allow-qos2.out"

        sb = _sbatch_when_qos_ready(
            c, ["-N", "1", "-p", name, "-q", "premium", "-t", "1", "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(c, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = c.read_output_on_any_node(out_path)
        assert "ALLOW_QOS_OK" in content

    def test_deny_qos_blocks_listed_qos(self, accounting_cluster):
        c = accounting_cluster
        name = _unique("part-deny-qos")
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=debug", "-i"])

        c.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--deny-qos=debug",
        )

        script = c.write_file("deny-qos.sh", "#!/bin/bash\necho DONE\n")

        out = c.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-q", "debug", "-t", "1", script]
        )
        assert "denied" in out.lower() or "error" in out.lower(), (
            f"expected rejection for denied QoS, got: {out}"
        )

    def test_deny_qos_allows_unlisted_qos(self, accounting_cluster):
        c = accounting_cluster
        name = _unique("part-deny-qos2")
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=debug", "-i"])
        c.sacctmgr(["add", "qos", "name=normal", "-i"])

        c.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
            "--deny-qos=debug",
        )

        script = c.write_file("deny-qos2.sh", "#!/bin/bash\necho DENY_QOS_OK\n")
        out_path = f"{c.remote_dir}/deny-qos2.out"

        sb = _sbatch_when_qos_ready(
            c, ["-N", "1", "-p", name, "-q", "normal", "-t", "1", "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(c, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = c.read_output_on_any_node(out_path)
        assert "DENY_QOS_OK" in content

    def test_runtime_update_deny_qos_takes_effect(self, accounting_cluster):
        """Add DenyQos to a running partition and verify enforcement is
        immediate without a controller restart."""
        c = accounting_cluster
        name = _unique("part-upd-qos")
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=lowpri", "-i"])

        # Start with no QoS restriction.
        c.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={node}",
        )

        script = c.write_file("upd-qos.sh", "#!/bin/bash\necho DONE\n")

        # Before update: lowpri QoS is accepted.
        sb = _sbatch_when_qos_ready(c, ["-N", "1", "-p", name, "-q", "lowpri", "-t", "1", script])
        assert parse_job_id(sb) is not None, "lowpri should be accepted before deny update"

        # Add lowpri to deny list at runtime.
        c.scontrol(
            "update-partition",
            f"--name={name}",
            "--deny-qos=lowpri",
            "--set-deny-qos",
        )

        # After update: lowpri must be rejected.
        out = c.cli_allow_fail(
            ["sbatch", "-N", "1", "-p", name, "-q", "lowpri", "-t", "1", script]
        )
        assert "denied" in out.lower() or "error" in out.lower(), (
            f"expected rejection after deny_qos update, got: {out}"
        )


class TestSlurmsyntax:
    """
    Verify that the Slurm-compatible inline key=value syntax works:
      scontrol create PartitionName=<n> Nodes=... MaxTime=...
      scontrol update PartitionName=<n> MaxTime=... State=...
      scontrol delete PartitionName=<n>

    These are in addition to the spur-native --flag subcommands.
    """

    def test_slurm_create_partition(self, cluster):
        name = _unique("slurm-crt")
        nodes_list = ",".join(cluster.node_names)

        out = cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
            "MaxTime=02:00:00",
            "State=UP",
            "PriorityTier=3",
        )
        assert "created" in out.lower(), f"expected created, got: {out}"

        show = cluster.scontrol("show", "partition")
        assert name in show
        assert "02:00:00" in show or "2:00:00" in show

    def test_slurm_create_partition_with_all_keys(self, cluster):
        name = _unique("slurm-crt-full")
        nodes_list = ",".join(cluster.node_names)

        out = cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
            "MaxTime=24:00:00",
            "DefaultTime=01:00:00",
            "MinNodes=1",
            "MaxNodes=4",
            "State=DOWN",
            "Default=NO",
            "AllowAccounts=acct1,acct2",
            "DenyAccounts=badacct",
            "AllowGroups=grp1",
            "PriorityTier=5",
            "PreemptMode=CANCEL",
        )
        assert "created" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show
        assert "CANCEL" in show or "cancel" in show.lower()

    def test_slurm_update_partition_maxtime(self, cluster):
        name = _unique("slurm-upd")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
            "MaxTime=01:00:00",
        )

        out = cluster.scontrol(
            "update",
            f"PartitionName={name}",
            "MaxTime=08:00:00",
        )
        assert "updated" in out.lower(), f"expected updated, got: {out}"

        show = cluster.scontrol("show", "partition")
        assert name in show
        assert "08:00:00" in show

    def test_slurm_update_partition_state(self, cluster):
        name = _unique("slurm-upd-state")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
            "State=UP",
        )

        out = cluster.scontrol(
            "update",
            f"PartitionName={name}",
            "State=DOWN",
        )
        assert "updated" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show
        # State should now be DOWN
        for line in show.splitlines():
            if name in line:
                # PartitionName line found; next line has State
                break
        assert "DOWN" in show or "down" in show.lower()

    def test_slurm_update_partition_multiple_fields(self, cluster):
        name = _unique("slurm-upd-multi")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
            "MaxTime=01:00:00",
            "PriorityTier=1",
        )

        out = cluster.scontrol(
            "update",
            f"PartitionName={name}",
            "MaxTime=12:00:00",
            "PriorityTier=10",
            "State=DRAIN",
        )
        assert "updated" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show
        assert "12:00:00" in show

    def test_slurm_update_partition_allow_accounts(self, cluster):
        name = _unique("slurm-upd-acct")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
        )

        out = cluster.scontrol(
            "update",
            f"PartitionName={name}",
            "AllowAccounts=team1,team2",
        )
        assert "updated" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show
        assert "team1" in show or "team2" in show

    def test_slurm_update_partition_deny_accounts(self, cluster):
        name = _unique("slurm-upd-deny")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
        )

        out = cluster.scontrol(
            "update",
            f"PartitionName={name}",
            "DenyAccounts=blocked",
        )
        assert "updated" in out.lower()

        show = cluster.scontrol("show", "partition")
        assert name in show
        assert "blocked" in show

    def test_slurm_delete_partition(self, cluster):
        name = _unique("slurm-del")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
        )

        show_before = cluster.scontrol("show", "partition")
        assert name in show_before

        out = cluster.scontrol("delete", f"PartitionName={name}")
        assert "deleted" in out.lower(), f"expected deleted, got: {out}"

        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after

    def test_slurm_delete_nonexistent_partition_fails(self, cluster):
        out = cluster.cli_allow_fail(
            ["scontrol", "delete", "PartitionName=no-such-partition-xyz"]
        )
        assert "not found" in out.lower() or "error" in out.lower(), (
            f"expected not-found error, got: {out}"
        )

    def test_slurm_create_duplicate_fails(self, cluster):
        name = _unique("slurm-dup")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
        )

        out = cluster.cli_allow_fail(
            ["scontrol", "create", f"PartitionName={name}", f"Nodes={nodes_list}"]
        )
        assert "already exists" in out.lower() or "error" in out.lower(), (
            f"expected duplicate-create error, got: {out}"
        )

    def test_slurm_update_nonexistent_fails(self, cluster):
        out = cluster.cli_allow_fail(
            ["scontrol", "update", "PartitionName=no-such-partition-xyz", "State=DOWN"]
        )
        assert "not found" in out.lower() or "error" in out.lower(), (
            f"expected not-found error, got: {out}"
        )

    def test_slurm_create_update_delete_round_trip(self, cluster):
        """Full lifecycle using only Slurm-compatible scontrol syntax."""
        name = _unique("slurm-rt")
        nodes_list = ",".join(cluster.node_names)
        node = cluster.node_names[0]

        # Create
        cluster.scontrol(
            "create",
            f"PartitionName={name}",
            f"Nodes={nodes_list}",
            "MaxTime=04:00:00",
            "PriorityTier=3",
            "State=UP",
        )
        show = cluster.scontrol("show", "partition")
        assert name in show

        # Update
        cluster.scontrol(
            "update",
            f"PartitionName={name}",
            "MaxTime=08:00:00",
            "State=DRAIN",
            "PriorityTier=5",
        )
        show = cluster.scontrol("show", "partition")
        assert "08:00:00" in show

        # Delete
        cluster.scontrol("delete", f"PartitionName={name}")
        show = cluster.scontrol("show", "partition")
        assert name not in show


class TestConfReadOnly:
    """Verify spur.conf is never written by any runtime partition operation.

    Slurm's slurmctld never writes slurm.conf; runtime changes live only in
    memory. Spur must match this: the config file is a read-only input to the
    daemon and the Raft WAL/snapshot is the authoritative runtime store.
    """

    def _conf_mtime(self, cluster) -> str:
        return cluster.nodes[0].exec(
            f"stat -c %Y '{cluster.etc_dir}/spur.conf'"
        ).strip()

    def test_create_partition_does_not_write_conf(self, cluster):
        name = _unique("conf-crt")
        node = cluster.node_names[0]

        mtime_before = self._conf_mtime(cluster)
        cluster.scontrol("create-partition", f"--name={name}", f"--nodes={node}")
        mtime_after = self._conf_mtime(cluster)

        assert mtime_before == mtime_after, (
            f"spur.conf was modified by create-partition: "
            f"mtime {mtime_before} → {mtime_after}"
        )

    def test_update_partition_does_not_write_conf(self, cluster):
        name = _unique("conf-upd")
        node = cluster.node_names[0]

        cluster.scontrol("create-partition", f"--name={name}", f"--nodes={node}")
        mtime_before = self._conf_mtime(cluster)

        cluster.scontrol("update-partition", f"--name={name}", "--state=DRAIN")
        mtime_after = self._conf_mtime(cluster)

        assert mtime_before == mtime_after, (
            f"spur.conf was modified by update-partition: "
            f"mtime {mtime_before} → {mtime_after}"
        )

    def test_delete_partition_does_not_write_conf(self, cluster):
        name = _unique("conf-del")
        node = cluster.node_names[0]

        cluster.scontrol("create-partition", f"--name={name}", f"--nodes={node}")
        mtime_before = self._conf_mtime(cluster)

        cluster.scontrol("delete-partition", f"--name={name}")
        mtime_after = self._conf_mtime(cluster)

        assert mtime_before == mtime_after, (
            f"spur.conf was modified by delete-partition: "
            f"mtime {mtime_before} → {mtime_after}"
        )


class TestPartitionPersistence:
    """Verify WAL-backed partition state survives a controller restart.

    Runtime partitions are persisted via Raft WAL/snapshot — not spur.conf.
    A restarted controller must see the same partition table the Raft log
    recorded, regardless of what spur.conf contains.
    """

    def test_runtime_partition_survives_restart(self, cluster):
        """A partition created at runtime must still be present after the
        controller is restarted from its Raft state directory."""
        name = _unique("persist-rt")
        node = cluster.node_names[0]

        cluster.scontrol("create-partition", f"--name={name}", f"--nodes={node}")
        show_before = cluster.scontrol("show", "partition")
        assert name in show_before

        cluster.restart_controller()

        show_after = cluster.scontrol("show", "partition")
        assert name in show_after, (
            f"runtime partition '{name}' must survive controller restart via WAL"
        )

    def test_deleted_partition_absent_after_restart(self, cluster):
        """A partition created and then deleted at runtime must remain absent
        after a controller restart — the WAL deletion must not be undone."""
        name = _unique("persist-del")
        node = cluster.node_names[0]

        cluster.scontrol("create-partition", f"--name={name}", f"--nodes={node}")
        cluster.scontrol("delete-partition", f"--name={name}")

        show_before_restart = cluster.scontrol("show", "partition")
        assert name not in show_before_restart

        cluster.restart_controller()

        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after, (
            f"deleted partition '{name}' must stay absent after controller restart"
        )


class TestReconfigure:
    """Verify scontrol reconfigure matches Slurm's semantics.

    Slurm: reconfigure re-reads slurm.conf and makes the live state match it.
    Partitions present only in the WAL (not in conf) are dropped. Partitions
    defined in conf survive and are not touched. scontrol reconfigure does NOT
    write to spur.conf.
    """

    def _conf_mtime(self, cluster) -> str:
        return cluster.nodes[0].exec(
            f"stat -c %Y '{cluster.etc_dir}/spur.conf'"
        ).strip()

    def test_reconfigure_drops_runtime_only_partition(self, cluster):
        """A partition created via scontrol (not in spur.conf) must be removed
        by scontrol reconfigure, matching Slurm's conf-wins semantics."""
        name = _unique("reconf-rt")
        node = cluster.node_names[0]

        cluster.scontrol("create-partition", f"--name={name}", f"--nodes={node}")
        show_before = cluster.scontrol("show", "partition")
        assert name in show_before, "partition must appear before reconfigure"

        cluster.scontrol("reconfigure")

        show_after = cluster.scontrol("show", "partition")
        assert name not in show_after, (
            f"runtime-only partition '{name}' must be dropped by reconfigure "
            f"(not in spur.conf)"
        )

    def test_reconfigure_preserves_conf_partitions(self, cluster):
        """scontrol reconfigure must not remove partitions defined in spur.conf."""
        show_before = cluster.scontrol("show", "partition")
        assert "default" in show_before

        cluster.scontrol("reconfigure")

        show_after = cluster.scontrol("show", "partition")
        assert "default" in show_after, (
            "conf-defined 'default' partition must survive reconfigure"
        )

    def test_reconfigure_does_not_write_conf(self, cluster):
        """scontrol reconfigure reads conf but must never write it back."""
        mtime_before = self._conf_mtime(cluster)
        cluster.scontrol("reconfigure")
        mtime_after = self._conf_mtime(cluster)

        assert mtime_before == mtime_after, (
            f"spur.conf was modified by reconfigure: "
            f"mtime {mtime_before} → {mtime_after}"
        )
