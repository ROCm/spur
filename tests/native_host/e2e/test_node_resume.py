# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Busy node resume keeps its allocation-derived state and running jobs."""

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state, wait_node_state, wait_until


def _node_fields(cluster, name):
    return dict(
        token.split("=", 1)
        for token in cluster.scontrol_show_node(name).split()
        if "=" in token
    )


def _assert_busy_nodes(cluster, jobs, cpus, expected):
    states = cluster.sinfo_nodes()
    short = "alloc" if expected == "ALLOCATED" else "mix"
    queue = cluster.squeue_all()
    for name, job_id in jobs.items():
        fields = _node_fields(cluster, name)
        assert fields["CPUTot"] == "4", fields
        assert fields["CPUAlloc"] == str(cpus), fields
        assert fields["State"] == expected, fields
        assert states[name] == short, states
        assert job_state(queue, job_id) == "R", queue


class TestBusyNodeResume:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"nodes": [{"names": "ALL", "cpus": 4, "memory_mb": 4096}]}

    @pytest.mark.parametrize("cpus,expected", [(4, "ALLOCATED"), (2, "MIXED")])
    @pytest.mark.parametrize("requested", ["RESUME", "IDLE"])
    def test_resume_preserves_busy_state(self, cluster, cpus, expected, requested):
        for name in cluster.node_names:
            if int(_node_fields(cluster, name)["CPUTot"]) < 4:
                pytest.skip("busy resume coverage requires at least 4 CPUs per node")
        jobs = {}
        try:
            for name in cluster.node_names:
                out = cluster.sbatch([
                    "-N", "1", "-w", name, "-c", str(cpus),
                    "-t", "10", "--wrap", "sleep 600",
                    "-o", f"{cluster.remote_dir}/resume-{name}.out",
                ])
                job_id = parse_job_id(out)
                assert job_id is not None, out
                jobs[name] = job_id
            for job_id in jobs.values():
                wait_job_state(cluster, job_id, "R")
            _assert_busy_nodes(cluster, jobs, cpus, expected)

            for name in jobs:
                cluster.scontrol("update", f"NodeName={name}",
                                 "State=DRAIN", "Reason=e2e-maintenance")
                assert _node_fields(cluster, name)["State"] == "DRAINING"
                cluster.scontrol("update", f"NodeName={name}", f"State={requested}")
            _assert_busy_nodes(cluster, jobs, cpus, expected)

            cluster.scontrol("update", "NodeName=ALL",
                             "State=DRAIN", "Reason=e2e-maintenance")
            for name in jobs:
                assert _node_fields(cluster, name)["State"] == "DRAINING"
            cluster.scontrol("update", "NodeName=ALL", f"State={requested}")
            _assert_busy_nodes(cluster, jobs, cpus, expected)

            cluster.restart_controller()
            _assert_busy_nodes(cluster, jobs, cpus, expected)
        finally:
            for job_id in jobs.values():
                cluster.cli_allow_fail(["scancel", str(job_id)])

        for name, job_id in jobs.items():
            assert wait_job(cluster, job_id) == "CA"
            wait_node_state(cluster, name, ["idle"])
            fields = _node_fields(cluster, name)
            assert fields["State"] == "IDLE", fields
            assert fields["CPUAlloc"] == "0", fields


class TestDrainCompletion:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"nodes": [{"names": "ALL", "cpus": 4, "memory_mb": 4096}]}

    def test_busy_node_reaches_drain_after_last_allocation_is_released(self, cluster):
        name = cluster.node_names[0]
        out = cluster.sbatch([
            "-N", "1", "-w", name, "-c", "2", "-t", "10",
            "--wrap", "sleep 600",
            "-o", f"{cluster.remote_dir}/drain-{name}.out",
        ])
        job_id = parse_job_id(out)
        assert job_id is not None, out

        try:
            wait_job_state(cluster, job_id, "R")
            assert _node_fields(cluster, name)["CPUAlloc"] == "2"

            cluster.scontrol("update", f"NodeName={name}",
                             "State=DRAIN", "Reason=e2e-maintenance")
            assert _node_fields(cluster, name)["State"] == "DRAINING"

            cluster.scancel(str(job_id))
            assert wait_job(cluster, job_id) == "CA"

            def drained_fields():
                fields = _node_fields(cluster, name)
                return fields if fields.get("State") == "DRAINED" else None

            fields = wait_until(
                drained_fields,
                timeout_s=60,
                desc=f"node {name} finishing drain",
            )
            assert fields["CPUAlloc"] == "0", fields
            assert fields["Reason"] == "e2e-maintenance", fields
            assert cluster.sinfo_nodes()[name] == "drain"
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])
            cluster.cli_allow_fail([
                "scontrol", "update", f"NodeName={name}", "State=RESUME",
            ])
