# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Group-node reuse preserves mandatory hosts and partition-specific placement."""

import re

import pytest

from cluster import parse_job_id, wait_job, wait_job_state, wait_until
from test_preemption_node_cap_reuse import _assert_blocked_for_scheduler_passes


_TEAM = "placement-reuse-team"
_QOS = "placement-reuse-pool"
_SCRIPT = "#!/bin/bash\nsleep 600\n"
_CAP_REASON = {"account": "AssocGrpNodeLimit", "qos": "QOSGrpNodeLimit"}


def _four_node_names(ssh_nodes):
    if len(ssh_nodes) < 4:
        pytest.skip("requires four nodes for occupied, mandatory, and idle controls")
    return [node.exec("hostname -s").strip() for node in ssh_nodes[:4]]


def _config(names, partitions):
    return {
        "nodes": [
            {"names": name, "cpus": 4, "memory_mb": 4096,
             "weight": 1000 if index == 3 else 1}
            for index, name in enumerate(names)
        ],
        "partitions": partitions,
        "scheduler": {"preempt_type": "none", "idle_fill_enabled": False},
        "auth": {"plugin": "none", "allow_root_jobs": True},
    }


def _partition(name, nodes, *, default=False, memory=None):
    partition = {
        "name": name,
        "state": "UP",
        "default": default,
        "nodes": ",".join(nodes),
        "max_time": "24:00:00",
        "default_time": "10:00",
    }
    if memory is not None:
        partition["def_mem_per_node"] = memory
    return partition


def _wait_caps(cluster, scope, cap):
    required = []
    if scope in ("account", "both"):
        required.append(("Account", _TEAM))
    if scope in ("qos", "both"):
        required.append(("QOS", _QOS))

    def visible():
        info = cluster.scontrol("show", "assoc_mgr")
        definitions = (("Account", _TEAM), ("QOS", _QOS))
        if not all(re.search(rf"(?m)^{kind}={name} ", info) for kind, name in definitions):
            return False
        return all(re.search(
            rf"(?m)^{kind}={name} [^\n]*\n[^\n]*GrpTRES=(?:[^\s,]+,)*node={cap}\(", info
        ) for kind, name in required)

    wait_until(visible, timeout_s=60, interval_s=1,
               desc="account and QOS definitions and group-node caps loaded")


def _setup(cluster, scope, cap):
    qos_args = ["add", "qos", f"name={_QOS}"]
    if scope in ("qos", "both"):
        qos_args.append(f"grptres=node={cap}")
    cluster.sacctmgr(qos_args)
    account_args = ["add", "account", f"name={_TEAM}"]
    if scope in ("account", "both"):
        account_args.append(f"grptres=node={cap}")
    cluster.sacctmgr(account_args)
    user_args = ["add", "user", f"name={cluster.nodes[0].user}",
                 f"account={_TEAM}", f"qos={_QOS}"]
    if scope in ("account", "both"):
        user_args.append(f"grptres=node={cap}")
    cluster.sacctmgr(user_args)
    _wait_caps(cluster, scope, cap)
    return cluster.write_file("node-reuse-placement.sh", _SCRIPT)


def _submit(cluster, submitted, script, name, nodes, nodelist, partition, *, memory=128):
    args = ["-J", name, "-N", str(nodes), "-n", str(nodes), "-c1",
            f"--nodelist={nodelist}", "-p", partition, "-A", _TEAM, "-q", _QOS]
    if memory is not None:
        args.append(f"--mem={memory}")
    output = cluster.sbatch(args + [script])
    job_id = parse_job_id(output)
    assert job_id is not None, output
    submitted.append(job_id)
    return job_id


def _assignment(cluster, job_id):
    show = cluster.scontrol("show", "job", str(job_id))
    nodes = re.search(r"(?<![A-Za-z])NodeList=(\S+)", show)
    partition = (re.search(r"\bEffectivePartition=(\S+)", show)
                 or re.search(r"\bPartition=(\S+)", show))
    assert nodes and partition, show
    expanded = cluster.scontrol("show", "hostnames", nodes.group(1)).splitlines()
    return set(expanded), partition.group(1)


def _cleanup(cluster, submitted):
    for job_id in submitted:
        cluster.cli_allow_fail(["scancel", str(job_id)])


@pytest.mark.parametrize("quota_scope", ["account", "qos"])
class TestAdditiveNodeReuse:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes):
        names = _four_node_names(ssh_nodes)
        return _config(names, [_partition("default", names, default=True)])

    def test_mandatory_new_host_uses_one_credit_and_requires_one_quota_slot(
        self, accounting_cluster, quota_scope
    ):
        c = accounting_cluster
        script = _setup(c, quota_scope, 3)
        n1, n2, mandatory, idle_control = c.node_names[:4]
        submitted = []
        try:
            holders = [
                _submit(c, submitted, script, f"reuse-holder-{index}", 1,
                        node, "default")
                for index, node in enumerate((n1, n2))
            ]
            for holder in holders:
                wait_job_state(c, holder, "R")
            incoming = _submit(c, submitted, script, "reuse-additive", 2,
                               mandatory, "default")
            wait_job_state(c, incoming, "R")
            assigned, partition = _assignment(c, incoming)
            assert partition == "default"
            assert mandatory in assigned and len(assigned) == 2, assigned
            assert len(assigned.intersection((n1, n2))) == 1, assigned
            assert idle_control not in assigned, assigned

            c.scancel(incoming)
            assert wait_job(c, incoming) in ("CA", "GONE")
            if quota_scope == "account":
                c.sacctmgr(["modify", "account", f"name={_TEAM}", "set", "grptres=node=2"])
                c.sacctmgr(["modify", "user", f"name={c.nodes[0].user}",
                            f"account={_TEAM}", "set", "grptres=node=2"])
            else:
                c.sacctmgr(["modify", "qos", f"name={_QOS}", "set", "grptres=node=2"])
            _wait_caps(c, quota_scope, 2)
            blocked = _submit(c, submitted, script, "reuse-mandatory-over-cap", 2,
                              mandatory, "default")
            wait_job_state(c, blocked, "PD")
            _assert_blocked_for_scheduler_passes(c, blocked, _CAP_REASON[quota_scope], holders)
        finally:
            _cleanup(c, submitted)


@pytest.mark.parametrize("other_memory", [128, 512], ids=["same-memory", "different-memory"])
@pytest.mark.parametrize("partition_order", ["default,other", "other,default"])
class TestPartitionNodeReuse:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes, other_memory):
        n1, n2, n3, n4 = _four_node_names(ssh_nodes)
        return _config([n1, n2, n3, n4], [
            _partition("default", [n1, n3], default=True, memory=128),
            _partition("other", [n2, n4], memory=other_memory),
        ])

    def test_each_partition_keeps_its_credit_and_can_fill_its_idle_host(
        self, accounting_cluster, other_memory, partition_order
    ):
        c = accounting_cluster
        script = _setup(c, "both", 3)
        n1, n2, n3, n4 = c.node_names[:4]
        submitted = []
        try:
            holders = [
                _submit(c, submitted, script, "reuse-default-holder", 1, n1, "default"),
                _submit(c, submitted, script, "reuse-other-holder", 1, n2, "other"),
            ]
            for holder in holders:
                wait_job_state(c, holder, "R")
            incoming = _submit(c, submitted, script, "reuse-partition-pool", 2,
                               ",".join((n1, n2, n3, n4)), partition_order, memory=None)
            wait_job_state(c, incoming, "R")
            assigned, partition = _assignment(c, incoming)
            expected = {"default": {n1, n3}, "other": {n2, n4}}
            assert partition in expected and assigned == expected[partition], (partition, assigned)
            for holder in holders:
                wait_job_state(c, holder, "R")
        finally:
            _cleanup(c, submitted)


class TestAdditivePartitionNodeReuse:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes):
        n1, n2, n3, n4 = _four_node_names(ssh_nodes)
        return _config([n1, n2, n3, n4], [
            _partition("default", [n1, n3, n4], default=True),
            _partition("other", [n2]),
        ])

    def test_partition_trials_cannot_drop_a_mandatory_host(self, accounting_cluster):
        c = accounting_cluster
        script = _setup(c, "both", 4)
        n1, n2 = c.node_names[:2]
        submitted = []
        try:
            holders = [
                _submit(c, submitted, script, "reuse-default-mandatory-holder", 1, n1, "default"),
                _submit(c, submitted, script, "reuse-other-mandatory-holder", 1, n2, "other"),
            ]
            for holder in holders:
                wait_job_state(c, holder, "R")
            incoming = _submit(c, submitted, script, "reuse-split-mandatory-hosts", 3,
                               ",".join((n1, n2)), "default,other", memory=None)
            wait_job_state(c, incoming, "PD")
            _assert_blocked_for_scheduler_passes(c, incoming, None, holders)
            show = c.scontrol("show", "job", str(incoming))
            assert re.search(r"(?<![A-Za-z])NodeList=\(null\)", show), show
            assert re.search(r"\bStartTime=N/A\b", show), show
        finally:
            _cleanup(c, submitted)
