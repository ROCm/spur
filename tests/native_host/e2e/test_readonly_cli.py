# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Smoke coverage for the read-only Slurm-compatible CLIs.

sprio, sstat, sdiag, and smd talk to the controller only. sshare and sreport
go through the accounting service, so they run against accounting_cluster.
These assert the contract a migrating Slurm script depends on: exit status,
header text, and that --noheader actually suppresses it.
"""

import re

import pytest

from cluster import parse_job_id, wait_job_state

pytestmark = pytest.mark.suite_api


def _run(cluster, args: list[str]) -> tuple[int, str]:
    return cluster.cli_with_env(args, {})


@pytest.fixture
def running_job(cluster):
    script = cluster.write_file("readonly-cli.sh", "#!/bin/bash\nsleep 120\n")
    job_id = parse_job_id(cluster.sbatch(["-J", "readonly-cli", script]))
    assert job_id is not None
    wait_job_state(cluster, job_id, "R", timeout=90)
    yield job_id
    cluster.cli_allow_fail(["scancel", str(job_id)])


class TestSprio:
    def test_prints_header(self, cluster):
        out = cluster.sprio()
        assert "JOBID" in out and "PRIORITY" in out, f"sprio header missing:\n{out}"
        assert "FAIRSHARE" in out, f"sprio header missing FAIRSHARE:\n{out}"

    def test_noheader_suppresses_the_header(self, cluster):
        assert "JOBID" not in cluster.sprio(["-h"]), "sprio -h must drop the header"
        assert "JOBID" not in cluster.sprio(["--noheader"])

    def test_long_form_adds_columns(self, cluster):
        out = cluster.sprio(["-l"])
        assert "QOS" in out and "EFFECTIVE" in out, (
            f"sprio -l must add QOS and EFFECTIVE:\n{out}"
        )

    def test_lists_a_pending_job(self, cluster):
        """sprio reports the pending queue, so a held job must appear."""
        script = cluster.write_file("sprio-held.sh", "#!/bin/bash\nsleep 60\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "sprio-held", "-H", script]))
        assert job_id is not None
        try:
            wait_job_state(cluster, job_id, "PD", timeout=30)
            out = cluster.sprio()
            assert str(job_id) in out, f"pending job {job_id} missing from sprio:\n{out}"
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_job_filter_excludes_other_jobs(self, cluster):
        code, out = _run(cluster, ["sprio", "-j", "99999999"])
        assert code == 0, f"an empty filter is not an error:\n{out}"
        assert "99999999" not in out


class TestSstat:
    def test_reports_a_running_job(self, cluster, running_job):
        code, out = _run(cluster, ["sstat", "-j", str(running_job)])
        assert code == 0, f"sstat failed:\n{out}"
        assert "JobID" in out, f"sstat header missing:\n{out}"
        assert str(running_job) in out, f"running job not in sstat output:\n{out}"

    def test_noheader_suppresses_the_header(self, cluster, running_job):
        code, out = _run(cluster, ["sstat", "-j", str(running_job), "--noheader"])
        assert code == 0, out
        assert "JobID" not in out, f"--noheader must drop the header:\n{out}"

    def test_parsable_output_is_pipe_delimited(self, cluster, running_job):
        code, out = _run(cluster, ["sstat", "-j", str(running_job), "-p"])
        assert code == 0, out
        assert "JobID|" in out, f"parsable header must be pipe-delimited:\n{out}"

    def test_format_selects_columns(self, cluster, running_job):
        code, out = _run(
            cluster, ["sstat", "-j", str(running_job), "-o", "jobid,ntasks"]
        )
        assert code == 0, out
        assert "NCPUS" not in out, f"-o must restrict the column set:\n{out}"

    def test_non_running_job_is_reported_not_fatal(self, cluster):
        script = cluster.write_file("sstat-held.sh", "#!/bin/bash\nsleep 60\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "sstat-held", "-H", script]))
        assert job_id is not None
        try:
            wait_job_state(cluster, job_id, "PD", timeout=30)
            code, out = _run(cluster, ["sstat", "-j", str(job_id)])
            assert code == 0, f"a pending job is not an sstat error:\n{out}"
            assert "is not running" in out, f"expected a diagnostic:\n{out}"
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_unknown_job_exits_nonzero(self, cluster):
        code, out = _run(cluster, ["sstat", "-j", "99999999"])
        assert code != 0, f"an unknown job must fail:\n{out}"

    def test_non_numeric_job_is_rejected(self, cluster):
        code, out = _run(cluster, ["sstat", "-j", "not-a-job"])
        assert code != 0, f"a non-numeric job id must fail:\n{out}"
        assert "no valid job IDs" in out, f"expected a specific message:\n{out}"

    def test_missing_job_argument_is_rejected(self, cluster):
        code, out = _run(cluster, ["sstat"])
        assert code != 0, f"-j is required:\n{out}"


class TestSdiag:
    def test_prints_all_sections(self, cluster):
        out = cluster.sdiag()
        for section in (
            "Server Information:",
            "Job Statistics:",
            "Node Statistics:",
            "Scheduler Statistics:",
            "Remote Procedure Call statistics by operation:",
        ):
            assert section in out, f"sdiag is missing {section!r}:\n{out}"

    def test_noheader_suppresses_the_banner(self, cluster):
        out = cluster.sdiag(["--noheader"])
        assert "sdiag output at" not in out, f"--noheader must drop the banner:\n{out}"
        assert "Server Information:" in out, (
            f"--noheader must not drop the report body:\n{out}"
        )

    def test_node_counts_match_the_cluster(self, cluster):
        out = cluster.sdiag()
        match = re.search(r"Total Nodes\s*:\s*(\d+)", out)
        assert match, f"sdiag did not report a node count:\n{out}"
        assert int(match.group(1)) == len(cluster.node_names)

    def test_reset_zeroes_accumulated_counters(self, cluster):
        """Only the submission counter is asserted at zero: the scheduler loop
        and agent heartbeats repopulate cycle and RPC counters immediately, so
        those can only be checked as a drop."""
        script = cluster.write_file("sdiag-job.sh", "#!/bin/bash\ntrue\n")
        for _ in range(2):
            cluster.sbatch(["-J", "sdiag-job", script])

        before = cluster.sdiag()
        submitted = int(re.search(r"Jobs submitted\s*:\s*(\d+)", before).group(1))
        cycles_before = int(re.search(r"Cycles\s*:\s*(\d+)", before).group(1))
        assert submitted > 0, f"expected submissions to be counted:\n{before}"

        cluster.sdiag(["--reset"])
        after = cluster.sdiag()
        assert int(re.search(r"Jobs submitted\s*:\s*(\d+)", after).group(1)) == 0, (
            f"--reset must zero the submission counter:\n{after}"
        )
        assert (
            int(re.search(r"Cycles\s*:\s*(\d+)", after).group(1)) < cycles_before
        ), f"--reset must drop the scheduler cycle count:\n{after}"


class TestSmd:
    def test_prints_a_health_report_for_every_node(self, cluster):
        out = cluster.smd()
        assert "=== Node Health Report" in out, f"smd banner missing:\n{out}"
        assert "NODE" in out and "FREE_MEM_MB" in out, f"smd header missing:\n{out}"
        for name in cluster.node_names:
            assert name in out, f"node {name} missing from smd:\n{out}"

    def test_healthy_cluster_reports_no_unhealthy_nodes(self, cluster):
        out = cluster.smd()
        assert f"All {len(cluster.node_names)} node(s) healthy" in out, (
            f"an idle cluster must be reported healthy:\n{out}"
        )

    def test_unhealthy_only_hides_healthy_nodes(self, cluster):
        out = cluster.smd(["-u"])
        for name in cluster.node_names:
            assert name not in out, (
                f"--unhealthy-only must hide healthy node {name}:\n{out}"
            )

    def test_drained_node_is_reported_unhealthy(self, cluster):
        target = cluster.node_names[0]
        cluster.scontrol("update", f"nodename={target}", "state=drain", "reason=smd-test")
        try:
            out = cluster.smd(["-u"])
            assert target in out, f"a drained node must show as unhealthy:\n{out}"
            assert "unhealthy node(s) detected" in out, f"expected a count:\n{out}"
        finally:
            cluster.scontrol("update", f"nodename={target}", "state=resume")


class TestSshare:
    def test_prints_header(self, accounting_cluster):
        out = accounting_cluster.sshare()
        assert "Account" in out and "FairShare" in out, (
            f"sshare header missing:\n{out}"
        )

    def test_noheader_suppresses_the_header(self, accounting_cluster):
        assert "FairShare" not in accounting_cluster.sshare(["-h"])

    def test_long_form_adds_group_limits(self, accounting_cluster):
        out = accounting_cluster.sshare(["-l"])
        assert "GrpCPUHrs" in out, f"sshare -l must add GrpCPUHrs:\n{out}"

    def test_lists_a_seeded_account(self, accounting_cluster):
        accounting_cluster.sacctmgr(["add", "account", "name=sharetest", "-i"])
        out = accounting_cluster.sshare()
        assert "sharetest" in out, f"seeded account missing from sshare:\n{out}"

    def test_account_filter_narrows_the_report(self, accounting_cluster):
        accounting_cluster.sacctmgr(["add", "account", "name=sharefilter", "-i"])
        accounting_cluster.sacctmgr(["add", "account", "name=shareother", "-i"])
        out = accounting_cluster.sshare(["-A", "sharefilter"])
        assert "sharefilter" in out, f"filtered account missing:\n{out}"
        assert "shareother" not in out, f"-A must exclude other accounts:\n{out}"


class TestSreport:
    def test_cluster_utilization_report(self, accounting_cluster):
        code, out = _run(
            accounting_cluster, ["sreport", "cluster", "AccountUtilizationByUser"]
        )
        assert code == 0, f"sreport failed:\n{out}"
        assert "CPU Hours" in out and "Account" in out, (
            f"utilization header missing:\n{out}"
        )

    def test_report_type_aliases_are_accepted(self, accounting_cluster):
        code, out = _run(
            accounting_cluster, ["sreport", "cluster", "userutilization"]
        )
        assert code == 0, f"alias was rejected:\n{out}"
        assert "User" in out and "CPU Hours" in out

    def test_job_sizes_report_has_a_total_row(self, accounting_cluster):
        code, out = _run(accounting_cluster, ["sreport", "job", "sizes"])
        assert code == 0, f"sreport job sizes failed:\n{out}"
        assert "% of Tot" in out, f"sizes header missing:\n{out}"
        assert "TOTAL" in out, f"sizes report must carry a total row:\n{out}"

    def test_noheader_suppresses_the_header(self, accounting_cluster):
        code, out = _run(
            accounting_cluster,
            ["sreport", "cluster", "AccountUtilizationByUser", "--noheader"],
        )
        assert code == 0, out
        assert "CPU Hours" not in out, f"--noheader must drop the header:\n{out}"

    def test_parsable_output_is_pipe_delimited(self, accounting_cluster):
        code, out = _run(
            accounting_cluster,
            ["sreport", "cluster", "AccountUtilizationByUser", "-p"],
        )
        assert code == 0, out
        assert "|" in out, f"parsable output must use a pipe delimiter:\n{out}"

    def test_unknown_report_type_is_rejected(self, accounting_cluster):
        code, out = _run(accounting_cluster, ["sreport", "cluster", "nonsense"])
        assert code != 0, f"an unknown report must fail:\n{out}"
        assert "unknown cluster report" in out, f"expected a specific message:\n{out}"

    def test_missing_subcommand_is_rejected(self, accounting_cluster):
        code, out = _run(accounting_cluster, ["sreport"])
        assert code != 0, f"sreport requires a subcommand:\n{out}"
