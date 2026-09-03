# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for preempt-exempt-time precedence.

The effective exempt window for a running job resolves QOS > partition > global,
returning on the first level that is set. Two consequences are easy to get wrong
and are covered here:

  - A QOS value wins outright, even when it is *shorter* than the partition's.
    It is an override, not a floor.
  - Clearing the QOS value must fall back to the partition, not to zero.

The distinction matters because a cluster whose global `preempt_exempt_time` is
unset resolves the bottom of that chain to 0 — no protection at all — so a QOS
override that fails to clear correctly silently strips the window rather than
restoring the partition default.

Existing coverage stops short of this: test_preemption_qos_hierarchy.py exercises
the *global* scheduler value and a partition value set via scontrol, but nothing
exercises the QOS level or the precedence between levels.

Requires:
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

# Partition window, deliberately far longer than the QOS override so the two
# levels cannot be confused for one another by a timing coincidence.
_PARTITION_EXEMPT_SECS = 90
# QOS override, short enough that the victim is eligible almost immediately.
_QOS_EXEMPT_SECS = 5

# Long enough to clear the 5s QOS window, short enough to stay well inside the
# 90s partition window — the gap between the two is what each test reads.
_SETTLE_SECS = 12
_GUARD_SECS = 15
_WAIT_PREEMPT = 60

# QOS cache refreshes on the accounting interval; a sacctmgr change needs a
# cycle before the scheduler acts on it.
_CACHE_WARMUP_SECS = 15

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
            "preempt_exempt_time": _PARTITION_EXEMPT_SECS,
        }
    ],
    "scheduler": {
        "preempt_type": "qos_priority",
    },
    # Required when the test runner SSHes in as root: spurd refuses to execute
    # jobs as uid 0 unless this is explicitly enabled.
    "auth": {"plugin": "none", "allow_root_jobs": True},
}


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = cluster.scontrol("show", "job", str(job_id))
    assert f"JobState={expected}" in show, (
        f"{label or 'job'} {job_id}: expected JobState={expected} in scontrol output:\n{show}"
    )


def _create_qos_pair(cluster) -> None:
    """Victim QOS plus a hunter QOS permitted to preempt it.

    The allow-list entry is applied by modify rather than at creation: CreateQos
    validates every name against the QOS table first, so the victim QOS has to
    exist before the hunter may reference it.
    """
    cluster.sacctmgr(["add", "qos", "name=exempt-victim", "priority=100",
                      "preemptmode=cancel"])
    cluster.sacctmgr(["add", "qos", "name=exempt-hunter", "priority=10000"])
    cluster.sacctmgr(["modify", "qos", "name=exempt-hunter", "set",
                      "preempt=exempt-victim"])


def _run_victim(cluster, node: str, prefix: str) -> int:
    script = cluster.write_file(f"{prefix}-victim.sh", _SLEEP_SCRIPT)
    victim_id = parse_job_id(
        cluster.sbatch([
            "-N1", "--exclusive", f"--nodelist={node}", "-q", "exempt-victim", script,
        ])
    )
    assert victim_id is not None, "victim submit failed"
    wait_job_state(cluster, victim_id, "R", timeout=30)
    _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")
    return victim_id


def _queue_hunter(cluster, node: str, prefix: str, script_body: str) -> int:
    script = cluster.write_file(f"{prefix}-hunter.sh", script_body)
    hunter_id = parse_job_id(
        cluster.sbatch([
            "-N1", "--exclusive", f"--nodelist={node}", "-q", "exempt-hunter", script,
        ])
    )
    assert hunter_id is not None, "hunter submit failed"
    return hunter_id


class TestQosExemptTimeOverridesPartition:
    """A QOS preemptexempttime shorter than the partition's must win.

    The victim is eligible once its own QOS window elapses, well before the
    partition would have released it. If the partition value were taking
    precedence the victim would survive, and this test fails.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_qos_exempt_time_wins_over_longer_partition_window(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        _create_qos_pair(c)
        c.sacctmgr(["modify", "qos", "name=exempt-victim", "set",
                    f"preemptexempttime={_QOS_EXEMPT_SECS}"])
        time.sleep(_CACHE_WARMUP_SECS)

        shown = c.sacctmgr(["show", "qos", "format=Name,PreemptExemptTime", "-P"])
        assert f"exempt-victim|{_QOS_EXEMPT_SECS}" in shown, (
            f"QOS preemptexempttime was not stored:\n{shown}"
        )

        victim_id = None
        hunter_id = None
        try:
            victim_id = _run_victim(c, node, "qos-wins")

            # Past the 5s QOS window, far short of the 90s partition window.
            time.sleep(_SETTLE_SECS)
            hunter_id = _queue_hunter(c, node, "qos-wins", _QUICK_SCRIPT)

            terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"victim should be preemptable {_SETTLE_SECS}s in, because its QOS "
                f"sets a {_QOS_EXEMPT_SECS}s window that overrides the partition's "
                f"{_PARTITION_EXEMPT_SECS}s; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, victim_id, "CANCELLED", "victim after preemption")

            wait_job_state(c, hunter_id, "R", timeout=30)
            _assert_scontrol_state(c, hunter_id, "RUNNING", "hunter after preemption")
        finally:
            for jid in (victim_id, hunter_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])


class TestClearQosExemptTimeRevertsToPartition:
    """clearpreemptexempttime must restore the partition window, not drop to zero.

    Falling through to the global default would be catastrophic on a cluster that
    leaves scheduler.preempt_exempt_time unset, since that resolves to 0 and the
    victim would be preemptable immediately.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_clearing_qos_exempt_time_falls_back_to_partition(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        _create_qos_pair(c)
        # Set, then clear: reaching the partition value by way of an override
        # proves the fallback, where never setting one would not.
        c.sacctmgr(["modify", "qos", "name=exempt-victim", "set",
                    f"preemptexempttime={_QOS_EXEMPT_SECS}"])
        c.sacctmgr(["modify", "qos", "name=exempt-victim", "set",
                    "clearpreemptexempttime=1"])
        time.sleep(_CACHE_WARMUP_SECS)

        shown = c.sacctmgr(["show", "qos", "format=Name,PreemptExemptTime", "-P"])
        assert "exempt-victim|" in shown and f"exempt-victim|{_QOS_EXEMPT_SECS}" not in shown, (
            f"QOS preemptexempttime should read blank after clearing:\n{shown}"
        )

        victim_id = None
        hunter_id = None
        try:
            victim_id = _run_victim(c, node, "qos-cleared")

            time.sleep(_SETTLE_SECS)
            hunter_id = _queue_hunter(c, node, "qos-cleared", _SLEEP_SCRIPT)
            wait_job_state(c, hunter_id, "PD", timeout=30)

            # Total elapsed stays well inside the 90s partition window, so the
            # victim must survive. Were the cleared QOS value falling through to
            # the global default of 0, it would already be gone.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "clearing the QOS exempt time must fall back to the partition's "
                f"{_PARTITION_EXEMPT_SECS}s window; the victim was evicted "
                f"~{_SETTLE_SECS + _GUARD_SECS}s in, which means the window "
                "collapsed to the global default of 0"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, hunter_id) == "PD", (
                "the hunter must wait out the partition exempt window"
            )
        finally:
            for jid in (victim_id, hunter_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
