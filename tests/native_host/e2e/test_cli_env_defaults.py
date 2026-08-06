# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for submit-time environment variable defaults.

sbatch and salloc read per-command env vars (SBATCH_*, SALLOC_*) and their
SPUR_* native twins as defaults for unset flags. These are submit-side
defaults read from the CLI's own environment, distinct from --export, which
shapes the job's environment.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job

TWO_PARTITIONS = {
    "partitions": [
        {
            "name": "default",
            "state": "UP",
            "default": True,
            "nodes": "ALL",
            "max_time": "24:00:00",
            "default_time": "10:00",
        },
        {
            "name": "envpart",
            "state": "UP",
            "nodes": "ALL",
            "max_time": "24:00:00",
            "default_time": "10:00",
        },
    ],
}


def _show_field(show_output: str, field: str) -> str:
    match = re.search(rf"\b{field}=(\S+)", show_output)
    assert match, f"missing {field} in scontrol output:\n{show_output}"
    return match.group(1)


class TestSbatchEnvDefaults:
    @pytest.fixture
    def cluster_config_overrides(self):
        return TWO_PARTITIONS

    def _submit(self, cluster, env, extra_args=None):
        script = cluster.write_file("env-default.sh", "#!/bin/bash\nsleep 1\n")
        code, out = cluster.cli_with_env(
            ["sbatch", "-J", "env-default"] + (extra_args or []) + [script], env
        )
        assert code == 0, f"sbatch failed (exit {code}):\n{out}"
        job_id = parse_job_id(out)
        assert job_id is not None, f"could not parse job id from:\n{out}"
        return job_id

    def test_spur_partition_twin_routes_job(self, cluster):
        job_id = self._submit(cluster, {"SPUR_PARTITION": "envpart"})
        show = cluster.scontrol("show", "job", str(job_id))
        assert _show_field(show, "Partition") == "envpart", (
            f"SPUR_PARTITION must select the partition:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)

    def test_sbatch_partition_env_routes_job(self, cluster):
        job_id = self._submit(cluster, {"SBATCH_PARTITION": "envpart"})
        show = cluster.scontrol("show", "job", str(job_id))
        assert _show_field(show, "Partition") == "envpart", (
            f"SBATCH_PARTITION must select the partition:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)

    def test_cli_flag_overrides_env_partition(self, cluster):
        job_id = self._submit(
            cluster, {"SPUR_PARTITION": "envpart"}, extra_args=["-p", "default"]
        )
        show = cluster.scontrol("show", "job", str(job_id))
        assert _show_field(show, "Partition") == "default", (
            f"an explicit -p must beat SPUR_PARTITION:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)

    def test_timelimit_env_applies(self, cluster):
        job_id = self._submit(cluster, {"SPUR_TIMELIMIT": "00:07:00"})
        show = cluster.scontrol("show", "job", str(job_id))
        assert "7" in _show_field(show, "TimeLimit"), (
            f"SPUR_TIMELIMIT must set the job time limit:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)

    def test_job_name_env_twin_is_not_inherited(self, cluster):
        """SPUR_JOB_NAME is injected into every running job, so sbatch must
        ignore it — otherwise a nested submit silently inherits the enclosing
        job's name. Only the SBATCH_-prefixed spelling is honored.
        """
        script = cluster.write_file("name-env.sh", "#!/bin/bash\nsleep 1\n")
        code, out = cluster.cli_with_env(
            ["sbatch", script], {"SPUR_JOB_NAME": "inherited"}
        )
        assert code == 0, f"sbatch failed (exit {code}):\n{out}"
        job_id = parse_job_id(out)
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _show_field(show, "JobName") != "inherited", (
            f"SPUR_JOB_NAME must not be inherited by sbatch:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)

    def test_sbatch_job_name_env_applies(self, cluster):
        script = cluster.write_file("name-env2.sh", "#!/bin/bash\nsleep 1\n")
        code, out = cluster.cli_with_env(
            ["sbatch", script], {"SBATCH_JOB_NAME": "from-env"}
        )
        assert code == 0, f"sbatch failed (exit {code}):\n{out}"
        job_id = parse_job_id(out)
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _show_field(show, "JobName") == "from-env", (
            f"SBATCH_JOB_NAME must set the job name:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)

    def test_env_output_path_applies(self, cluster):
        out_path = f"{cluster.remote_dir}/env-output.out"
        script = cluster.write_file(
            "env-output.sh", "#!/bin/bash\necho MARKER_FROM_ENV_OUTPUT\n"
        )
        code, out = cluster.cli_with_env(
            ["sbatch", script], {"SPUR_OUTPUT": out_path}
        )
        assert code == 0, f"sbatch failed (exit {code}):\n{out}"
        job_id = parse_job_id(out)
        assert job_id is not None

        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)
        content = cluster.read_output_on_any_node(out_path)
        assert "MARKER_FROM_ENV_OUTPUT" in content, (
            f"SPUR_OUTPUT must redirect job output to {out_path}, got:\n{content!r}"
        )


class TestSallocEnvDefaults:
    """salloc spawns $SHELL inside the allocation.

    Pointing SHELL at a probe script turns that into a synchronous assertion:
    the script runs while the allocation is live, records what salloc injected,
    and exits, which releases the allocation.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return TWO_PARTITIONS

    def _run_salloc(self, cluster, env, extra_args=None):
        probe_out = f"{cluster.remote_dir}/salloc-probe.out"
        probe = cluster.write_file(
            "salloc-probe.sh",
            "#!/bin/bash\n"
            f'echo "partition=$SPUR_JOB_PARTITION" > {probe_out}\n'
            f'echo "qos=$SPUR_JOB_QOS" >> {probe_out}\n'
            f'echo "jobid=$SPUR_JOB_ID" >> {probe_out}\n',
        )
        code, out = cluster.cli_with_env(
            ["salloc"] + (extra_args or []), {**env, "SHELL": probe}
        )
        assert code == 0, f"salloc failed (exit {code}):\n{out}"
        content = cluster.read_output_on_any_node(probe_out)
        assert content.strip(), f"salloc probe produced no output; salloc said:\n{out}"
        return dict(
            line.split("=", 1)
            for line in content.splitlines()
            if "=" in line
        )

    def test_spur_partition_twin_applies(self, cluster):
        values = self._run_salloc(cluster, {"SPUR_PARTITION": "envpart"})
        assert values["partition"] == "envpart", (
            f"SPUR_PARTITION must select the salloc partition, got {values}"
        )

    def test_salloc_partition_env_applies(self, cluster):
        values = self._run_salloc(cluster, {"SALLOC_PARTITION": "envpart"})
        assert values["partition"] == "envpart", (
            f"SALLOC_PARTITION must select the salloc partition, got {values}"
        )

    def test_cli_flag_overrides_env_partition(self, cluster):
        values = self._run_salloc(
            cluster, {"SALLOC_PARTITION": "envpart"}, extra_args=["-p", "default"]
        )
        assert values["partition"] == "default", (
            f"an explicit -p must beat SALLOC_PARTITION, got {values}"
        )


class TestSallocQos:
    """An explicit QOS must exist in the controller's cache to be accepted,
    so these need the accounting fixture."""

    def _run_salloc(self, cluster, env, extra_args=None):
        probe_out = f"{cluster.remote_dir}/salloc-qos.out"
        probe = cluster.write_file(
            "salloc-qos-probe.sh",
            "#!/bin/bash\n" f'echo "qos=$SPUR_JOB_QOS" > {probe_out}\n',
        )
        code, out = cluster.cli_with_env(
            ["salloc"] + (extra_args or []), {**env, "SHELL": probe}
        )
        assert code == 0, f"salloc failed (exit {code}):\n{out}"
        content = cluster.read_output_on_any_node(probe_out)
        assert content.strip(), f"salloc probe produced no output; salloc said:\n{out}"
        return content.split("qos=", 1)[1].strip()

    def test_salloc_qos_flag_and_env(self, accounting_cluster):
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=allocqos"])
        # QoS cache refreshes on the fairshare interval (10s in e2e config).
        time.sleep(15)

        assert self._run_salloc(c, {}, extra_args=["-q", "allocqos"]) == "allocqos", (
            "salloc --qos must reach the job spec"
        )
        assert self._run_salloc(c, {"SALLOC_QOS": "allocqos"}) == "allocqos", (
            "SALLOC_QOS must default the QOS"
        )
        assert self._run_salloc(c, {"SPUR_QOS": "allocqos"}) == "allocqos", (
            "the SPUR_QOS twin must default the QOS"
        )
