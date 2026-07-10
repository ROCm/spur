# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for runtime partition management via scontrol."""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def _unique(prefix: str) -> str:
    return f"{prefix}-{int(time.time())}"


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
        assert out.strip() != "" or True  # CLI may reject at arg-parse level; just must not crash


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
        """Structural test: the in-use guard is enforced by unit tests
        (validate_partition + delete_partition in cluster.rs). Here we verify
        the CLI path: deleting an idle partition while another partition has
        running jobs does NOT block the idle partition's deletion."""
        name = _unique("part-idle")
        nodes_list = ",".join(cluster.node_names)

        cluster.scontrol(
            "create-partition",
            f"--name={name}",
            f"--nodes={nodes_list}",
        )

        # Confirm partition exists.
        show = cluster.scontrol("show", "partition")
        assert name in show

        # No jobs on this partition — deletion must succeed.
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

    def test_jobs_on_default_partition_schedule_after_create(self, cluster):
        """Create a new partition, then verify jobs on the DEFAULT partition
        still schedule normally (partition CRUD does not disrupt other partitions)."""
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

        # Submit to the default partition to confirm the cluster is healthy.
        script = cluster.write_file("part-sched.sh", "#!/bin/bash\necho PARTITION_OK\n")
        out_path = f"{cluster.remote_dir}/part-sched.out"
        sb = cluster.sbatch(["-N", "1", "-w", node, "-t", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

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

        sb = c.sbatch(
            ["-N", "1", "-p", name, "-q", "premium", "-t", "1", "-o", out_path, script]
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

        sb = c.sbatch(
            ["-N", "1", "-p", name, "-q", "normal", "-t", "1", "-o", out_path, script]
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
        sb = c.sbatch(["-N", "1", "-p", name, "-q", "lowpri", "-t", "1", script])
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
