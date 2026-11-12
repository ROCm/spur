# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Occupied-host caps must admit reuse before preemption checks free capacity.

The two-host fixture scales a five-host entitlement down to one: the team
already charges its full node cap, and an incoming job needs resources occupied
by burst work on that same host. Both account and QOS group caps are exercised.
"""

import re
import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state, wait_until


_HOLD_SCRIPT = "#!/bin/bash\nsleep 600\n"
_TEAM = "reuse-team"
_BURST_ACCOUNT = "reuse-borrower"
_TEAM_QOS = "reuse-entitled"
_BURST_QOS = "reuse-burst"
_CAP_REASON = {"account": "AssocGrpNodeLimit", "qos": "QOSGrpNodeLimit"}


def _reason(cluster, job_id):
    return cluster.squeue(["-j", str(job_id), "-h", "-o", "%r"]).strip()


def _assert_node(cluster, job_id, expected):
    show = cluster.scontrol("show", "job", str(job_id))
    match = re.search(r"(?<![A-Za-z])NodeList=(\S+)", show)
    assert match and match.group(1) == expected, show


def _assert_fully_busy(cluster, node):
    show = cluster.scontrol_show_node(node)
    assert re.search(r"\bState=ALLOCATED\b", show), show
    assert re.search(r"\bCPUAlloc=4\b", show), show


def _assert_blocked_for_scheduler_passes(cluster, job_id, reason, holders):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if _reason(cluster, job_id) == reason:
            break
        time.sleep(1)
    assert _reason(cluster, job_id) == reason, cluster.scontrol("show", "job", str(job_id))
    # Several one-second scheduling passes must observe the same restriction.
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        queue = cluster.squeue_all()
        assert job_state(queue, job_id) == "PD", queue
        assert _reason(cluster, job_id) == reason, cluster.scontrol("show", "job", str(job_id))
        for holder in holders:
            assert job_state(queue, holder) == "R", queue
        time.sleep(1)


@pytest.mark.parametrize("quota_scope", ["account", "qos"])
class TestPreemptionNodeCapReuse:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "nodes": [{"names": "ALL", "cpus": 4, "memory_mb": 4096}],
            "partitions": [{
                "name": "default",
                "state": "UP",
                "default": True,
                "nodes": "ALL",
                "max_time": "24:00:00",
                "default_time": "10:00",
                "preempt_mode": "cancel",
            }],
            "scheduler": {"preempt_type": "qos_priority"},
            "auth": {"plugin": "none", "allow_root_jobs": True},
        }

    def _setup(self, cluster, quota_scope, *, charged_holder_evictable=False):
        if len(cluster.node_names) < 2:
            pytest.skip("requires two nodes: one charged host and one additional host")
        for node in cluster.node_names[:2]:
            show = cluster.scontrol_show_node(node)
            assert re.search(r"\bCPUTot=4\b", show), show
        preempt = _BURST_QOS
        if charged_holder_evictable and quota_scope == "qos":
            preempt += f",{_TEAM_QOS}"
        team_qos_args = [
            "add", "qos", f"name={_TEAM_QOS}", "priority=10000",
            "preemptmode=cancel",
        ]
        if quota_scope == "qos":
            team_qos_args.append("grptres=node=1")
        cluster.sacctmgr(team_qos_args)
        cluster.sacctmgr([
            "add", "qos", f"name={_BURST_QOS}", "priority=100", "preemptmode=cancel",
        ])
        cluster.sacctmgr(["modify", "qos", f"name={_TEAM_QOS}", "set", f"preempt={preempt}"])
        account_args = ["add", "account", f"name={_TEAM}"]
        if quota_scope == "account":
            account_args.append("grptres=node=1")
        cluster.sacctmgr(account_args)
        cluster.sacctmgr(["add", "account", f"name={_BURST_ACCOUNT}"])
        user = cluster.nodes[0].user
        team_user_args = [
            "add", "user", f"name={user}", f"account={_TEAM}",
            f"qos={_TEAM_QOS},{_BURST_QOS}",
        ]
        if quota_scope == "account":
            team_user_args.append("grptres=node=1")
        cluster.sacctmgr(team_user_args)
        cluster.sacctmgr([
            "add", "user", f"name={user}", f"account={_BURST_ACCOUNT}",
            f"qos={_BURST_QOS}",
        ])

        def policies_visible():
            info = cluster.scontrol("show", "assoc_mgr")
            names = (("QOS", _TEAM_QOS), ("QOS", _BURST_QOS),
                     ("Account", _TEAM), ("Account", _BURST_ACCOUNT))
            if not all(re.search(rf"(?m)^{kind}={name} ", info) for kind, name in names):
                return False
            kind, name = ("Account", _TEAM) if quota_scope == "account" else ("QOS", _TEAM_QOS)
            return bool(re.search(
                rf"(?m)^{kind}={name} [^\n]*\n[^\n]*GrpTRES=node=1\(0\)", info
            ))

        wait_until(policies_visible, timeout_s=60, interval_s=1,
                   desc="account and QOS definitions loaded into scheduler caches")
        return cluster.write_file("reuse-hold.sh", _HOLD_SCRIPT)

    @staticmethod
    def _submit(cluster, submitted, script, name, node, cpus, account, qos):
        output = cluster.sbatch([
            "-J", name, "-N1", "-c", str(cpus), "--mem=256",
            f"--nodelist={node}", "-A", account, "-q", qos, script,
        ])
        job_id = parse_job_id(output)
        assert job_id is not None, output
        submitted.append(job_id)
        return job_id

    @staticmethod
    def _cleanup(cluster, submitted):
        for job_id in submitted:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_protected_charged_sibling_reports_resources_then_reuses_host(
        self, accounting_cluster, quota_scope
    ):
        """Partial-node eviction remains blocked, but the node quota must permit reuse."""
        c = accounting_cluster
        script = self._setup(c, quota_scope)
        node = c.node_names[0]
        submitted = []
        try:
            holder = self._submit(c, submitted, script, "reuse-holder", node, 1, _TEAM, _TEAM_QOS)
            wait_job_state(c, holder, "R")
            burst = self._submit(
                c, submitted, script, "reuse-burst", node, 3, _BURST_ACCOUNT, _BURST_QOS
            )
            wait_job_state(c, burst, "R")
            _assert_fully_busy(c, node)
            before = c.sdiag_jobs_preempted()
            incoming = self._submit(
                c, submitted, script, "reuse-incoming", node, 3, _TEAM, _TEAM_QOS
            )
            wait_job_state(c, incoming, "PD")
            _assert_blocked_for_scheduler_passes(c, incoming, "Resources", [holder, burst])
            assert c.sdiag_jobs_preempted() == before

            c.scancel(burst)
            assert wait_job(c, burst) in ("CA", "GONE")
            wait_job_state(c, incoming, "R")
            assert job_state(c.squeue_all(), holder) == "R"
            _assert_node(c, incoming, node)
        finally:
            self._cleanup(c, submitted)

    def test_already_charged_host_can_be_reused_through_preemption(
        self, accounting_cluster, quota_scope
    ):
        """A fully eligible victim set reaches preemption while the cap is occupied."""
        c = accounting_cluster
        script = self._setup(c, quota_scope, charged_holder_evictable=True)
        node = c.node_names[0]
        holder_qos = _BURST_QOS if quota_scope == "account" else _TEAM_QOS
        submitted = []
        try:
            holder = self._submit(c, submitted, script, "reuse-holder", node, 1, _TEAM, holder_qos)
            wait_job_state(c, holder, "R")
            burst = self._submit(
                c, submitted, script, "reuse-burst", node, 3, _BURST_ACCOUNT, _BURST_QOS
            )
            wait_job_state(c, burst, "R")
            _assert_fully_busy(c, node)
            before = c.sdiag_jobs_preempted()
            incoming = self._submit(
                c, submitted, script, "reuse-incoming", node, 3, _TEAM, _TEAM_QOS
            )
            for victim in (holder, burst):
                assert wait_job(c, victim) in ("CA", "GONE")
            wait_job_state(c, incoming, "R")
            _assert_node(c, incoming, node)
            assert c.sdiag_jobs_preempted() == before + 2
        finally:
            self._cleanup(c, submitted)

    def test_additional_host_is_blocked_until_charged_host_is_released(
        self, accounting_cluster, quota_scope
    ):
        """Potential preemption cannot credit a host outside the group's occupied set."""
        c = accounting_cluster
        script = self._setup(c, quota_scope)
        charged_node, additional_node = c.node_names[:2]
        submitted = []
        try:
            holder = self._submit(
                c, submitted, script, "reuse-holder", charged_node, 1, _TEAM, _TEAM_QOS
            )
            wait_job_state(c, holder, "R")
            burst = self._submit(
                c, submitted, script, "reuse-burst", additional_node, 4,
                _BURST_ACCOUNT, _BURST_QOS,
            )
            wait_job_state(c, burst, "R")
            _assert_fully_busy(c, additional_node)
            before = c.sdiag_jobs_preempted()
            incoming = self._submit(
                c, submitted, script, "reuse-additional", additional_node, 3, _TEAM, _TEAM_QOS
            )
            wait_job_state(c, incoming, "PD")
            _assert_blocked_for_scheduler_passes(
                c, incoming, _CAP_REASON[quota_scope], [holder, burst]
            )
            assert c.sdiag_jobs_preempted() == before

            c.scancel(holder)
            assert wait_job(c, holder) in ("CA", "GONE")
            assert wait_job(c, burst) in ("CA", "GONE")
            wait_job_state(c, incoming, "R")
            _assert_node(c, incoming, additional_node)
            assert c.sdiag_jobs_preempted() == before + 1
        finally:
            self._cleanup(c, submitted)
