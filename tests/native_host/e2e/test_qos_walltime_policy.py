# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end coverage for partition and quality-of-service wall-time policy."""

import re
import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def _pending_reason(cluster, job_id: int) -> str:
    show = cluster.scontrol("show", "job", str(job_id))
    match = re.search(r"Reason=(\S+)", show)
    return match.group(1) if match else ""


def _sbatch_when_qos_ready(cluster, args: list[str], timeout: int = 15) -> str:
    deadline = time.time() + timeout
    while True:
        try:
            return cluster.sbatch(args)
        except RuntimeError as error:
            if "does not exist" not in str(error) or time.time() >= deadline:
                raise
            time.sleep(1)


def _wait_time_limit(cluster, job_id: int, timeout: int = 30) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        limit = cluster.squeue(
            ["-j", str(job_id), "-h", "-o", "%l", "-t", "all"]
        ).strip()
        if limit:
            return limit
        time.sleep(1)
    raise AssertionError(f"job {job_id} never appeared in squeue within {timeout}s")


def _wait_pending_reason(
    cluster, job_id: int, expected: str, timeout: int = 30
) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _pending_reason(cluster, job_id) == expected:
            return
        time.sleep(1)
    raise AssertionError(
        f"job {job_id} did not receive {expected!r}; "
        f"last reason was {_pending_reason(cluster, job_id)!r}"
    )


class TestPartitionAndQosWallTimePolicy:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "scheduler": {
                "default_time_limit_minutes": 0,
                "enforce_part_limits": "ALL",
            },
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "00:02:00",
                }
            ],
        }

    def test_partition_ceiling_with_long_and_standard_qos(self, accounting_cluster):
        cluster = accounting_cluster
        cluster.sacctmgr(["add", "qos", "name=long-window", "maxwall=2"])
        cluster.sacctmgr(["add", "qos", "name=standard-window", "maxwall=1"])

        script = cluster.write_file("qos-walltime-policy.sh", "#!/bin/bash\nsleep 5\n")

        long_job = parse_job_id(
            _sbatch_when_qos_ready(
                cluster,
                ["-J", "long-window", "-N", "1", "-q", "long-window", script],
            )
        )
        assert long_job is not None
        assert _wait_time_limit(cluster, long_job) == "2:00"
        assert wait_job(cluster, long_job, timeout=30) == "CD"

        standard_job = parse_job_id(
            _sbatch_when_qos_ready(
                cluster,
                ["-J", "standard-window", "-N", "1", "-q", "standard-window", script],
            )
        )
        assert standard_job is not None
        assert _wait_time_limit(cluster, standard_job) == "1:00"
        assert wait_job(cluster, standard_job, timeout=30) == "CD"

        over_partition = cluster.cli_allow_fail(
            [
                "sbatch",
                "-J",
                "over-partition-wall",
                "-N",
                "1",
                "-q",
                "long-window",
                "-t",
                "3",
                script,
            ]
        )
        assert "Requested time limit is invalid" in over_partition, over_partition

        short_job = parse_job_id(
            _sbatch_when_qos_ready(
                cluster,
                [
                    "-J",
                    "standard-over-wall",
                    "-N",
                    "1",
                    "-q",
                    "standard-window",
                    "-t",
                    "2",
                    script,
                ],
            )
        )
        assert short_job is not None
        try:
            wait_job_state(cluster, short_job, "PD", timeout=30)
            _wait_pending_reason(cluster, short_job, "QOSMaxWallDurationPerJobLimit")
        finally:
            cluster.cli_allow_fail(["scancel", str(short_job)])
