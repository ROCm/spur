# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for accounting: sacct exit reporting and QoS pending reasons.

Requires Postgres on node 0 (the accounting_cluster fixture, which skips
when Docker is unavailable).
"""

import re
import time

from cluster import deep_merge, parse_job_id, wait_job, wait_job_state, wait_sacct_row


class TestSacctExitReporting:
    def test_signal_half_and_derived_exit_code(self, accounting_cluster):
        c = accounting_cluster

        # (1) A job killed by a signal: sacct must show the signal half (0:9),
        # not 0:0. SIGKILL the batch shell itself.
        sig = c.write_file("acct-signal.sh", "#!/bin/bash\nkill -9 $$\n")
        sig_id = parse_job_id(c.sbatch(["-J", "acct-sig", "-N", "1", sig]))
        assert sig_id is not None
        wait_job(c, sig_id, timeout=60)
        row = wait_sacct_row(c, sig_id, "JobID,ExitCode")
        # ExitCode renders code:signal; the signal half is the parity fix.
        assert row.split()[1].endswith(":9"), f"expected signal half :9, got {row!r}"

        # (2) A multi-step job (steps exit 0, 7, 3): Slurm reports
        # ExitCode=last (3:0) and DerivedExitCode=max (7:0).
        multi = c.write_file(
            "acct-multi.sh",
            "#!/bin/bash\n"
            "srun bash -c 'exit 0'\n"
            "srun bash -c 'exit 7'\n"
            "srun bash -c 'exit 3'\n",
        )
        m_id = parse_job_id(c.sbatch(["-J", "acct-multi", "-N", "1", multi]))
        assert m_id is not None
        wait_job(c, m_id, timeout=90)
        row = wait_sacct_row(c, m_id, "JobID,ExitCode,DerivedExitCode")
        fields = row.split()
        assert fields[1] == "3:0", f"expected ExitCode 3:0, got {fields!r}"
        assert fields[2] == "7:0", f"expected DerivedExitCode 7:0, got {fields!r}"


def _reason(cluster, job_id: int) -> str:
    out = cluster.scontrol("show", "job", str(job_id))
    m = re.search(r"Reason=(\S+)", out)
    return m.group(1) if m else ""


class TestSacctmgrShowQos:
    """Verify sacctmgr show qos displays TRES/node-allocation fields, honors
    format= selection, and filters by where name=."""

    def test_default_output_shows_tres_columns(self, accounting_cluster):
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=nodeqos", "priority=50",
                     "grptres=node=4,cpu=16", "maxtresperjob=node=2"])
        time.sleep(15)
        out = c.sacctmgr(["show", "qos"])
        assert "nodeqos" in out
        assert "node=4" in out, f"GrpTRES node limit missing: {out!r}"
        assert "node=2" in out, f"MaxTRES node limit missing: {out!r}"

    def test_format_selects_specific_fields(self, accounting_cluster):
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=fmtqos", "priority=10",
                     "grptres=cpu=32", "maxtresperjob=cpu=8"])
        time.sleep(15)
        out = c.sacctmgr(["show", "qos", "format=Name,GrpTRES,MaxTRES"])
        assert "fmtqos" in out
        assert "cpu=32" in out, f"GrpTRES missing: {out!r}"
        assert "cpu=8" in out, f"MaxTRES missing: {out!r}"
        # Priority should NOT appear since it was not in the format list.
        lines = [l for l in out.splitlines() if "fmtqos" in l]
        assert lines, f"no fmtqos row in output: {out!r}"
        assert "Priority" not in out.splitlines()[0], (
            f"Priority column should not appear: {out!r}")

    def test_where_name_filters_to_one_qos(self, accounting_cluster):
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=alpha", "priority=1"])
        c.sacctmgr(["add", "qos", "name=beta", "priority=2"])
        time.sleep(15)
        out = c.sacctmgr(["show", "qos", "where", "name=alpha"])
        assert "alpha" in out
        assert "beta" not in out, f"name filter did not exclude beta: {out!r}"

    def test_show_qos_renders_all_limit_columns(self, accounting_cluster):
        c = accounting_cluster

        c.sacctmgr(
            [
                "add",
                "qos",
                "name=fullcap",
                "maxjobsperuser=2",
                "maxsubmitjobsperuser=4",
                "maxwall=30",
                "grpwall=60",
                "maxtresperjob=cpu=8",
                "maxtresperuser=cpu=16",
                "grptres=cpu=32",
            ]
        )
        time.sleep(15)

        out = c.sacctmgr(["show", "qos"])
        header, *rows = [line for line in out.splitlines() if line.strip()]
        for column in (
            "MaxJobsPU",
            "MaxSubmitPU",
            "MaxWall",
            "GrpWall",
            "MaxTRES",
            "MaxTRESPU",
            "GrpTRES",
        ):
            assert column in header, f"missing column {column!r} in header: {header!r}"

        row = next((r for r in rows if r.startswith("fullcap")), None)
        assert row is not None, f"fullcap row not found in: {rows!r}"
        assert "2" in row.split()
        assert "4" in row.split()
        assert "30" in row.split()
        assert "60" in row.split()
        assert "cpu=8" in row
        assert "cpu=16" in row
        assert "cpu=32" in row


class TestQosLimitReasons:
    def test_wall_cap_sets_qos_pending_reason(self, accounting_cluster):
        c = accounting_cluster

        # Define a QoS that caps wall time at 1 minute.
        c.sacctmgr(["add", "qos", "name=short", "maxwall=1"])
        # Wait past the QoS cache refresh floor (10s) before submitting, else the job starts before the cap loads.
        time.sleep(15)

        # A job in that QoS asking for 1h exceeds the cap, so it stays PENDING
        # with the specific QoS reason (not generic Resources/PartitionTimeLimit).
        script = c.write_file("qos-job.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(["-J", "qos-wall", "-N", "1", "-q", "short", "-t", "60", script])
        )
        assert job_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxWallDurationPerJobLimit":
                break
            time.sleep(2)
        assert reason == "QOSMaxWallDurationPerJobLimit", (
            f"expected QOSMaxWallDurationPerJobLimit, got {reason!r}"
        )

    def test_cluster_default_qos_binds_and_enforces_no_qos_job(self, accounting_cluster):
        # A job submitted with no -q must be bound to the configured cluster
        # fallback QOS and subject to its limits, closing the "omit --qos to
        # run unenforced" bypass.
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=capped", "maxwall=1"])
        # Merge the fallback into the on-disk config, then restart so spurctld
        # re-reads it (restart_controller alone does not re-render the config).
        deep_merge(c.config_overrides, {"accounting": {"default_qos": "capped"}})
        c._write_config()
        c.restart_controller()
        time.sleep(15)

        script = c.write_file("nodefault.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(["-J", "no-qos", "-N", "1", "-t", "60", script])
        )
        assert job_id is not None

        show = c.scontrol("show", "job", str(job_id))
        assert "QOS=capped" in show, f"no-qos job not bound to fallback: {show!r}"

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxWallDurationPerJobLimit":
                break
            time.sleep(2)
        assert reason == "QOSMaxWallDurationPerJobLimit", (
            f"fallback QOS limit not enforced, got {reason!r}"
        )

    def test_node_cap_sets_qos_pending_reason(self, accounting_cluster):
        c = accounting_cluster

        # Define a QoS that caps a user to 1 node.
        c.sacctmgr(["add", "qos", "name=nodecap", "maxtresperuser=node=1"])
        time.sleep(15)

        # A job in that QoS asking for 2 nodes exceeds the per-user cap.
        script = c.write_file("qos-node-job.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(["-J", "qos-node", "-N", "2", "-q", "nodecap", script])
        )
        assert job_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxNodePerUserLimit":
                break
            time.sleep(2)
        assert reason == "QOSMaxNodePerUserLimit", (
            f"expected QOSMaxNodePerUserLimit, got {reason!r}"
        )

    def test_memory_cap_sets_qos_pending_reason(self, accounting_cluster):
        c = accounting_cluster

        # Define a QoS that caps a user to 1G of memory.
        c.sacctmgr(["add", "qos", "name=memcap", "maxtresperuser=mem=1024"])
        time.sleep(15)

        # A job in that QoS asking for 2G exceeds the per-user cap.
        script = c.write_file("qos-mem-job.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(
                ["-J", "qos-mem", "-N", "1", "--mem=2G", "-q", "memcap", script]
            )
        )
        assert job_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxMemoryPerUser":
                break
            time.sleep(2)
        assert reason == "QOSMaxMemoryPerUser", (
            f"expected QOSMaxMemoryPerUser, got {reason!r}"
        )

    def test_memory_per_cpu_cap_sets_qos_pending_reason(self, accounting_cluster):
        c = accounting_cluster

        # Same cap as test_memory_cap_sets_qos_pending_reason, but the job
        # requests memory via --mem-per-cpu instead of --mem: the QOS memory
        # check must derive the same effective total (2 CPUs * 1024 MB = 2G)
        # rather than treating a --mem-per-cpu job as requesting 0 memory.
        c.sacctmgr(["add", "qos", "name=memcappercpu", "maxtresperuser=mem=1024"])
        time.sleep(15)

        script = c.write_file("qos-mem-per-cpu-job.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(
                [
                    "-J",
                    "qos-mem-per-cpu",
                    "-N",
                    "1",
                    "-c",
                    "2",
                    "--mem-per-cpu=1024",
                    "-q",
                    "memcappercpu",
                    script,
                ]
            )
        )
        assert job_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxMemoryPerUser":
                break
            time.sleep(2)
        assert reason == "QOSMaxMemoryPerUser", (
            f"expected QOSMaxMemoryPerUser, got {reason!r}"
        )

    def test_gpu_cap_sets_qos_pending_reason(self, accounting_cluster):
        c = accounting_cluster

        # Define a QoS that caps a user to 2 GPUs.
        c.sacctmgr(["add", "qos", "name=gpucap", "maxtresperuser=gres/gpu=2"])
        time.sleep(15)

        # A job in that QoS asking for 4 GPUs exceeds the per-user cap. The QOS
        # limit check is independent of physical GPU availability, so this tags
        # the pending reason regardless of the node's actual GPU count.
        script = c.write_file("qos-gpu-job.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(
                ["-J", "qos-gpu", "-N", "1", "--gres=gpu:4", "-q", "gpucap", script]
            )
        )
        assert job_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxGRESPerUser":
                break
            time.sleep(2)
        assert reason == "QOSMaxGRESPerUser", (
            f"expected QOSMaxGRESPerUser, got {reason!r}"
        )

    def test_grp_cpu_cap_not_oversubscribed_by_concurrent_jobs(self, accounting_cluster):
        # A QOS group cap must hold across a burst of submissions, not
        # just one job. Three 1-cpu jobs each fit the grptres=cpu=2 cap on their
        # own, but a single scheduling pass must never run more than two at once
        # — the third pends with QOSGrpCpuLimit until one finishes.
        c = accounting_cluster

        # 1-cpu jobs against a cpu=2 group cap: needs >= 2 schedulable CPUs, which
        # the provisioned test nodes always have, so physical CPU is never the
        # binding constraint here — only the QOS group cap is.
        c.sacctmgr(["add", "qos", "name=grpburst", "grptres=cpu=2"])
        time.sleep(15)

        script = c.write_file("qos-grpburst.sh", "#!/bin/bash\nsleep 30\n")
        ids = []
        for i in range(3):
            job_id = parse_job_id(
                c.sbatch(["-J", "grpburst", "-N", "1", "-c", "1", "-q", "grpburst", script])
            )
            assert job_id is not None
            ids.append(job_id)

        # Poll for the steady state and assert the cap is never breached: at most
        # two of the three run concurrently, and the surplus shows QOSGrpCpuLimit.
        deadline = time.time() + 40
        running = []
        while time.time() < deadline:
            running = c.running_job_ids_by_name("grpburst")
            assert len(running) <= 2, (
                f"grptres=cpu=2 over-subscribed: {len(running)} jobs running {running}"
            )
            if len(running) == 2:
                break
            time.sleep(2)
        assert len(running) == 2, f"expected 2 jobs running under the cap, got {running}"

        blocked = [j for j in ids if j not in running]
        assert len(blocked) == 1, f"expected exactly one blocked job, got {blocked}"
        assert _reason(c, blocked[0]) == "QOSGrpCpuLimit", (
            f"blocked job {blocked[0]} reason: {_reason(c, blocked[0])!r}"
        )


class TestSacctmgrUserAssociationLimits:
    def test_maxjobs_set_via_add_user_blocks_a_second_job(self, accounting_cluster):
        c = accounting_cluster
        user = c.nodes[0].user

        # `sacctmgr add user ... maxjobs=1` is the real write path this test
        # closes the gap on: previously the only way to set this limit was
        # raw SQL against the associations table.
        c.sacctmgr(["add", "account", "name=assoccap"])
        c.sacctmgr(["add", "user", f"name={user}", "account=assoccap", "maxjobs=1"])
        # Wait past the association cache refresh floor (10s) before
        # submitting, else the job starts before the cap loads.
        time.sleep(15)

        script = c.write_file("assoc-maxjobs.sh", "#!/bin/bash\nsleep 30\n")
        first_id = parse_job_id(
            c.sbatch(["-J", "assoc-first", "-N", "1", "-A", "assoccap", script])
        )
        assert first_id is not None
        wait_job_state(c, first_id, "R", timeout=30)

        second_id = parse_job_id(
            c.sbatch(["-J", "assoc-second", "-N", "1", "-A", "assoccap", script])
        )
        assert second_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, second_id)
            if reason == "AssocMaxJobsLimit":
                break
            time.sleep(2)
        assert reason == "AssocMaxJobsLimit", f"expected AssocMaxJobsLimit, got {reason!r}"


class TestSacctmgrQosAuthorization:
    """A user submitting an explicit --qos outside their association's
    pinned default QOS must be rejected (SPUR-101), not silently accepted."""

    def test_submission_rejects_qos_outside_association_default(self, accounting_cluster):
        c = accounting_cluster
        user = c.nodes[0].user

        c.sacctmgr(["add", "qos", "name=authqos"])
        c.sacctmgr(["add", "qos", "name=otherqos"])
        c.sacctmgr(["add", "account", "name=qosauthz"])
        c.sacctmgr(["add", "user", f"name={user}", "account=qosauthz", "defaultqos=authqos"])
        # Wait past the association cache refresh floor (10s) before
        # submitting, else the pinned default hasn't loaded yet.
        time.sleep(15)

        script = c.write_file("qos-authz.sh", "#!/bin/bash\ntrue\n")

        out = c.cli_allow_fail(
            ["sbatch", "-J", "qos-bad", "-N", "1", "-A", "qosauthz", "--qos=otherqos", script]
        )
        assert "not permitted" in out, f"expected an authorization rejection, got {out!r}"

        good_id = parse_job_id(
            c.sbatch(["-J", "qos-good", "-N", "1", "-A", "qosauthz", "--qos=authqos", script])
        )
        assert good_id is not None
        wait_job(c, good_id, timeout=30)

    def test_submission_rejects_borrowing_qos_from_a_different_account(self, accounting_cluster):
        # A user in two accounts, each pinned to its own QOS, must not be
        # able to submit under one account while borrowing the other's QOS
        # — the exact cross-account confusion reported in SPUR-101.
        c = accounting_cluster
        user = c.nodes[0].user

        c.sacctmgr(["add", "qos", "name=hyperloomqos"])
        c.sacctmgr(["add", "qos", "name=primusqos"])
        c.sacctmgr(["add", "account", "name=hyperloom"])
        c.sacctmgr(["add", "account", "name=primus"])
        c.sacctmgr(["add", "user", f"name={user}", "account=hyperloom", "defaultqos=hyperloomqos"])
        c.sacctmgr(["add", "user", f"name={user}", "account=primus", "defaultqos=primusqos"])
        time.sleep(15)

        script = c.write_file("qos-cross-acct.sh", "#!/bin/bash\ntrue\n")

        out = c.cli_allow_fail(
            [
                "sbatch",
                "-J",
                "qos-borrow",
                "-N",
                "1",
                "-A",
                "hyperloom",
                "--qos=primusqos",
                script,
            ]
        )
        assert "not permitted" in out, f"expected an authorization rejection, got {out!r}"

    def test_submission_allows_any_member_of_the_qos_allow_list(self, accounting_cluster):
        # sacctmgr add/modify user qos=a,b grants a set, not just one pinned
        # default — mirrors Slurm's per-association QOS allow-list.
        c = accounting_cluster
        user = c.nodes[0].user

        c.sacctmgr(["add", "qos", "name=burstqos"])
        c.sacctmgr(["add", "qos", "name=normalqos"])
        c.sacctmgr(["add", "qos", "name=otherqos"])
        c.sacctmgr(["add", "account", "name=qoslist"])
        c.sacctmgr(
            [
                "add",
                "user",
                f"name={user}",
                "account=qoslist",
                "qos=burstqos,normalqos",
                "defaultqos=normalqos",
            ]
        )
        time.sleep(15)

        script = c.write_file("qos-list.sh", "#!/bin/bash\ntrue\n")

        first_id = parse_job_id(
            c.sbatch(["-J", "qos-list-a", "-N", "1", "-A", "qoslist", "--qos=burstqos", script])
        )
        assert first_id is not None
        wait_job(c, first_id, timeout=30)

        second_id = parse_job_id(
            c.sbatch(["-J", "qos-list-b", "-N", "1", "-A", "qoslist", "--qos=normalqos", script])
        )
        assert second_id is not None
        wait_job(c, second_id, timeout=30)

        out = c.cli_allow_fail(
            ["sbatch", "-J", "qos-list-bad", "-N", "1", "-A", "qoslist", "--qos=otherqos", script]
        )
        assert "not permitted" in out, f"expected an authorization rejection, got {out!r}"


class TestSacctmgrInvalidInput:
    def test_add_qos_with_non_numeric_limit_fails_cleanly(self, accounting_cluster):
        c = accounting_cluster

        # A typo'd numeric limit must be rejected outright, not silently
        # coerced to 0 ("unlimited").
        out = c.cli_allow_fail(["sacctmgr", "add", "qos", "name=badlimit", "maxjobsperuser=abc"])
        assert "maxjobsperuser" in out, f"expected error mentioning maxjobsperuser, got {out!r}"

        show_out = c.sacctmgr(["show", "qos"])
        assert "badlimit" not in show_out, "QOS should not have been created"

    def test_add_qos_with_unit_suffixed_tres_fails_cleanly(self, accounting_cluster):
        c = accounting_cluster

        # Slurm's K/M/G unit-suffix TRES syntax isn't supported; it must be
        # rejected rather than silently dropped into a no-op limit.
        out = c.cli_allow_fail(
            ["sacctmgr", "add", "qos", "name=badtres", "maxtresperjob=mem=1G"]
        )
        assert "mem" in out, f"expected error mentioning the bad TRES token, got {out!r}"

        show_out = c.sacctmgr(["show", "qos"])
        assert "badtres" not in show_out, "QOS should not have been created"
