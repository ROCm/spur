# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end test for hoisting an in-flight preemptor to the front of
placement.

Eviction eligibility is decided on the QOS-base axis (a preemptor's QOS must
allow-list and outrank the victim's QOS), while placement of a freed node is
decided on the effective-priority axis (base x fair-share x age x tier). Because
the two axes differ, a bystander that loses the eviction axis can still win the
placement axis — a higher fair-share on a lower QOS base — and steal the node a
preemption just freed, leaving the preemptor pending to evict again.

The fix hoists an in-flight preemptor ahead of such a bystander in the pending
list, so the backfill scheduler reserves the freed node for the preemptor rather
than handing it to whoever sorts highest on effective priority.

This test reproduces exactly that shape:

  - victim: low QOS, running on the single node, allow-listed as preemptable.
  - preemptor: high QOS that allow-lists and outranks the victim, so it evicts.
  - bystander: a lower QOS base than the preemptor but a maximal fair-share
    advantage, giving it a higher effective priority. It cannot preempt (its QOS
    allow-lists nothing), so it has no claim to the node the preemption frees.

The assertion: the preemptor runs on the freed node and the bystander stays
pending. Without the hoist the bystander would win the node on effective
priority.

Requires:
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"

_GUARD_SECS = 15

# fairshare_refresh_secs is 10 in the harness config and FairshareCache clamps
# its interval to a 10s floor, so a planted usage row needs two cycles plus
# slack before the scheduler is guaranteed to see it. The same wait covers the
# QOS cache, which refreshes on the same interval.
_FAIRSHARE_REFRESH_SECS = 30

# Required when the test runner SSHes in as root: spurd refuses to execute jobs
# as uid 0 unless this is explicitly enabled.
_AUTH_ROOT = {"auth": {"plugin": "none", "allow_root_jobs": True}}

_BASE_CONFIG = {
    "partitions": [
        {
            "name": "default",
            "state": "UP",
            "default": True,
            "nodes": "ALL",
            "max_time": "24:00:00",
            "default_time": "10:00",
            "preempt_mode": "cancel",
        }
    ],
    "scheduler": {
        "preempt_type": "qos_priority",
    },
    **_AUTH_ROOT,
}

# Planted usage large enough that the account's actual share saturates, and
# small enough (the counterpart) to hit the fair-share epsilon. Together they
# drive the two accounts to opposite ends of the fair-share range.
_HEAVY_USAGE = 999_000_000
_LIGHT_USAGE = 1


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    show = cluster.scontrol("show", "job", str(job_id))
    assert f"JobState={expected}" in show, (
        f"{label or 'job'} {job_id}: expected JobState={expected} in scontrol output:\n{show}"
    )


def _sql_str(value: str) -> str:
    return value.replace("'", "''")


def _seed_usage(cluster, user: str, account: str, cpu_seconds: int) -> None:
    cluster.psql(
        "INSERT INTO usage "
        "(user_name, account, period_start, period_end, cpu_seconds, gpu_seconds, job_count) "
        f"VALUES ('{_sql_str(user)}', '{_sql_str(account)}', "
        f"date_trunc('day', NOW()), NOW(), {cpu_seconds}, 0, 1) "
        "ON CONFLICT (user_name, account, period_start) "
        "DO UPDATE SET cpu_seconds = EXCLUDED.cpu_seconds"
    )


class TestPreemptorHoistBeatsHigherEffectivePriorityBystander:
    """The node a preemption frees goes to the preemptor, not to a fair-share-
    favoured bystander that could never have evicted its occupant.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_freed_node_goes_to_preemptor_not_bystander(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        # The bystander's account is driven to the top of the fair-share range;
        # the preemptor's to the bottom, so only the hoist (not fair-share) can
        # make the preemptor win the freed node.
        c.sacctmgr(["add", "account", "name=hoist-heavy", "fairshare=1"])
        c.sacctmgr(["add", "account", "name=hoist-light", "fairshare=10"])
        c.sacctmgr(["add", "user", f"name={user}", "account=hoist-heavy"])
        c.sacctmgr(["add", "user", f"name={user}", "account=hoist-light"])
        _seed_usage(c, user, "hoist-heavy", _HEAVY_USAGE)
        _seed_usage(c, user, "hoist-light", _LIGHT_USAGE)

        # victim: preemptable. preemptor: outranks and allow-lists the victim.
        # bystander: a lower base than the preemptor, allow-lists nothing.
        c.sacctmgr(["add", "qos", "name=hoist-victim", "priority=100", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=hoist-preemptor", "priority=10000",
                    "preempt=hoist-victim"])
        c.sacctmgr(["add", "qos", "name=hoist-bystander", "priority=5000",
                    "preemptmode=cancel"])
        time.sleep(_FAIRSHARE_REFRESH_SECS)

        victim_id = None
        preemptor_id = None
        bystander_id = None
        try:
            victim_script = c.write_file("hoist-victim.sh", _SLEEP_SCRIPT)
            victim_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "hoist-light", "-q", "hoist-victim", victim_script,
                ])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            preempted_before = c.sdiag_jobs_preempted()

            # The bystander arrives first and from the pristine-fair-share
            # account, so on effective priority it outranks the preemptor. It
            # targets the same node but cannot evict the victim.
            bystander_script = c.write_file("hoist-bystander.sh", _SLEEP_SCRIPT)
            bystander_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "hoist-light", "-q", "hoist-bystander", bystander_script,
                ])
            )
            assert bystander_id is not None, "bystander submit failed"
            wait_job_state(c, bystander_id, "PD", timeout=30)

            # The preemptor arrives from the exhausted-fair-share account, so it
            # is behind the bystander on effective priority. It evicts the victim
            # and must then win the node it freed.
            preemptor_script = c.write_file("hoist-preemptor.sh", _SLEEP_SCRIPT)
            preemptor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "hoist-heavy", "-q", "hoist-preemptor", preemptor_script,
                ])
            )
            assert preemptor_id is not None, "preemptor submit failed"

            # The preemptor runs: it evicted the victim and claimed the node.
            wait_job_state(c, preemptor_id, "R", timeout=60)
            _assert_scontrol_state(c, preemptor_id, "RUNNING", "preemptor")
            assert c.sdiag_jobs_preempted() > preempted_before, (
                "the preemptor reached RUNNING but the scheduler recorded no "
                "preemption; it must have won the node some other way and this "
                "test would otherwise pass for the wrong reason"
            )

            # The bystander stays pending across a guard window: its higher
            # effective priority must not win the node the preemption freed.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, bystander_id) == "PD", (
                "the fair-share-favoured bystander must stay pending; the node "
                "freed by the preemption belongs to the preemptor that freed it"
            )
            _assert_scontrol_state(c, bystander_id, "PENDING", "bystander after guard")
            assert job_state(sq, preemptor_id) == "R", "preemptor must still be running"
        finally:
            for jid in (victim_id, preemptor_id, bystander_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
