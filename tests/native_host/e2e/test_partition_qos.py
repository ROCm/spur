# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
A partition QOS caps one user across every account and job QOS they submit with.

The scenario is a user in two accounts, each pinned to its own team QOS with no
cap of its own, submitting exclusive one-node jobs from both. Only the partition
QOS sees both accounts' jobs together.

Requires Postgres on node 0 (accounting_cluster), three nodes, and a node with
four GPUs for the GPU cap.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job_state

_SLEEP = "#!/bin/bash\nsleep 900\n"
_CACHE_REFRESH = 15
_GUARD_SECS = 15


def _config(*, idle_fill: bool = False) -> dict:
    base = {
        "state": "UP",
        "nodes": "ALL",
        "max_time": "24:00:00",
        "default_time": "30:00",
        "preempt_mode": "off",
    }
    return {
        "partitions": [
            {**base, "name": "capped", "default": True, "qos": "usercap"},
            {**base, "name": "open"},
        ],
        "scheduler": {
            "idle_fill_enabled": idle_fill,
            "idle_fill_exempt_secs": 5,
        },
        "auth": {"plugin": "none", "allow_root_jobs": True},
    }


def _require_nodes(cluster, count: int) -> None:
    if len(cluster.node_names) < count:
        pytest.skip(f"needs {count} nodes, inventory has {len(cluster.node_names)}")


def _two_accounts(cluster, team_qos_limits: str = "") -> None:
    user = cluster.nodes[0].user
    for team in ("qa", "qb"):
        cluster.sacctmgr(["add", "qos", f"name={team}", *team_qos_limits.split()])
    cluster.sacctmgr(["add", "account", "name=acct-a"])
    cluster.sacctmgr(["add", "account", "name=acct-b"])
    cluster.sacctmgr(["add", "user", f"name={user}", "account=acct-a", "qos=qa", "defaultqos=qa"])
    cluster.sacctmgr(["add", "user", f"name={user}", "account=acct-b", "qos=qb", "defaultqos=qb"])


def _submit(cluster, name: str, account: str, *extra: str, shape=("-N1", "--exclusive")) -> int:
    qos = {"acct-a": "qa", "acct-b": "qb"}[account]
    script = cluster.write_file(f"{name}.sh", _SLEEP)
    out = cluster.sbatch(
        [*shape, "-A", account, f"--qos={qos}", f"--job-name={name}", *extra, script]
    )
    job_id = parse_job_id(out)
    assert job_id is not None, f"{name} submit failed: {out}"
    return job_id


def _reason(cluster, job_id: int) -> str:
    show = cluster.scontrol("show", "job", str(job_id))
    for field in show.split():
        if field.startswith("Reason="):
            return field.removeprefix("Reason=")
    return show


def _partition_of(cluster, job_id: int) -> str:
    show = cluster.scontrol("show", "job", str(job_id))
    for field in show.split():
        if field.startswith("EffectivePartition="):
            return field.removeprefix("EffectivePartition=")
    return ""


def _idle_nodes(cluster) -> int:
    return sum(1 for s in cluster.sinfo_nodes().values() if s.lower().startswith("idle"))


def _stays_pending(cluster, job_id: int, reason: str) -> None:
    deadline = time.time() + 60
    state = job_state(cluster.squeue_all(), job_id)
    while state != "PD" and time.time() < deadline:
        time.sleep(2)
        state = job_state(cluster.squeue_all(), job_id)
    assert state == "PD", f"job {job_id} must pend, but is {state}:\n{cluster.squeue_all()}"
    time.sleep(_GUARD_SECS)
    assert job_state(cluster.squeue_all(), job_id) == "PD", (
        f"job {job_id} must stay pending:\n{cluster.squeue_all()}"
    )
    got = _reason(cluster, job_id)
    assert got == reason, f"job {job_id}: expected Reason={reason}, got {got}"


def _cancel(cluster, job_ids) -> None:
    for job_id in job_ids:
        cluster.cli_allow_fail(["scancel", str(job_id)])


class TestPartitionQosCapsAUserAcrossAccounts:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config()

    def test_an_undefined_partition_qos_holds_jobs_until_it_exists(self, accounting_cluster):
        c = accounting_cluster
        _two_accounts(c)
        time.sleep(_CACHE_REFRESH)
        ids = []
        try:
            held = _submit(c, "held", "acct-a")
            ids.append(held)
            _stays_pending(c, held, "InvalidQOS")

            c.sacctmgr(["add", "qos", "name=usercap", "maxtresperuser=node=2"])
            wait_job_state(c, held, "R", timeout=90)
        finally:
            _cancel(c, ids)

    def test_one_user_is_capped_across_two_accounts(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 3)
        _two_accounts(c)
        c.sacctmgr(["add", "qos", "name=usercap", "maxtresperuser=node=2"])
        time.sleep(_CACHE_REFRESH)
        ids = []
        try:
            first = _submit(c, "a1", "acct-a")
            ids.append(first)
            wait_job_state(c, first, "R", timeout=60)

            second = _submit(c, "b1", "acct-b")
            ids.append(second)
            wait_job_state(c, second, "R", timeout=60)

            third = _submit(c, "a2", "acct-a")
            ids.append(third)
            _stays_pending(c, third, "QOSMaxNodePerUserLimit")
            assert _idle_nodes(c) > 0, "a free node must exist, so only the cap holds the job"
            assert "QoS=usercap" in c.scontrol("show", "partition", "capped")

            either = _submit(c, "b2", "acct-b", "-p", "capped,open")
            ids.append(either)
            wait_job_state(c, either, "R", timeout=60)
            assert _partition_of(c, either) == "open", "the capped partition must be dropped"

            c.cli_allow_fail(["scancel", str(first)])
            wait_job_state(c, third, "R", timeout=90)

            fourth = _submit(c, "b3", "acct-b")
            ids.append(fourth)
            c.cli_allow_fail(["scancel", str(either)])
            _stays_pending(c, fourth, "QOSMaxNodePerUserLimit")

            c.scontrol("update", "PartitionName=capped", "QOS=")
            wait_job_state(c, fourth, "R", timeout=90)
            assert "QoS=N/A" in c.scontrol("show", "partition", "capped")

            c.scontrol("update", "PartitionName=capped", "QOS=usercap")
            assert "QoS=usercap" in c.scontrol("show", "partition", "capped")
            fifth = _submit(c, "a3", "acct-a")
            ids.append(fifth)
            _stays_pending(c, fifth, "QOSMaxNodePerUserLimit")
        finally:
            _cancel(c, ids)


class TestPartitionQosCapsGpusAcrossAccounts:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config()

    def test_one_users_gpus_are_capped_across_two_accounts(self, accounting_cluster):
        c = accounting_cluster
        gpus = max(c.node_gpu_count(name) for name in c.node_names)
        if gpus < 4:
            pytest.skip(f"needs a node with 4 GPUs, the largest has {gpus}")
        _two_accounts(c)
        c.sacctmgr(["add", "qos", "name=usercap", "maxtresperuser=gres/gpu=3"])
        time.sleep(_CACHE_REFRESH)
        shape = ("-N1", "--gres=gpu:2")
        ids = []
        try:
            first = _submit(c, "ga", "acct-a", shape=shape)
            ids.append(first)
            wait_job_state(c, first, "R", timeout=60)

            second = _submit(c, "gb", "acct-b", shape=shape)
            ids.append(second)
            _stays_pending(c, second, "QOSMaxGRESPerUser")

            c.cli_allow_fail(["scancel", str(first)])
            wait_job_state(c, second, "R", timeout=90)
        finally:
            _cancel(c, ids)


class TestIdleFillDoesNotLendPastThePartitionCap:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _config(idle_fill=True)

    def test_a_job_over_its_team_cap_is_not_lent_past_the_user_cap(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 2)
        _two_accounts(c, "grptres=node=1")
        c.sacctmgr(["add", "qos", "name=usercap", "maxtresperuser=node=1"])
        time.sleep(_CACHE_REFRESH)
        ids = []
        try:
            legit = _submit(c, "legit", "acct-a")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            over = _submit(c, "over", "acct-a")
            ids.append(over)
            _stays_pending(c, over, "QOSMaxNodePerUserLimit")
            assert _idle_nodes(c) > 0, "a free node must exist, so lending was possible"
        finally:
            _cancel(c, ids)
