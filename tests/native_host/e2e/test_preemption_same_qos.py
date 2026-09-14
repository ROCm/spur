# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for preemption between two jobs in the *same* QOS.

Under preempt_type=qos_priority a pending job may only evict a running job when
the pending job's QOS lists the running job's QOS in its `preempt` allow-list and
outranks it. Spur carves out the identical-QOS case from the rank comparison —
where a strict rank test could never hold — so a QOS naming *itself* is what
enables same-QOS preemption.

This differs from Slurm, where the allow-list is consulted only on the
different-QOS branch and same-QOS preemption is gated by a dedicated
PreemptMode=WITHIN flag; there, self-listing is a no-op.

Every test here holds the QOS rank constant and varies only the allow-list, so a
passing result can only be explained by the allow-list. Both jobs are additionally
submitted with a large raw priority boost on the pending side, which the current
policy ignores entirely — if raw job priority ever regains influence over
preemption eligibility, these tests fail loudly rather than drift.

Requires:
  - preempt_type=qos_priority (scheduler config) so allow-list gating applies
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 60
_GUARD_SECS = 12

# QOS cache is refreshed on the same interval as the other accounting caches;
# a freshly added or modified QOS needs a cycle before the scheduler sees it.
_CACHE_WARMUP_SECS = 15

# Raw job priority is not part of the eligibility rule. It is boosted anyway so
# that a regression re-introducing a job-priority gate is caught here.
_AGGRESSOR_PRIORITY = 1_000_000

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


def _start_pair(cluster, qos: str, node: str, prefix: str):
    """Run a victim under *qos*, then queue an aggressor in the same QOS behind
    it and boost the aggressor's raw job priority far past the victim's.

    Returns (victim_id, aggressor_id, preempted_before) with the victim RUNNING.
    preempted_before is sampled while only the victim exists: once the aggressor
    is queued the scheduler may act on it immediately, so a later sample can
    already include this pair's own preemption.

    The caller asserts the aggressor's state — under a self-listing QOS it may
    never be observably PENDING.
    """
    victim_script = cluster.write_file(f"{prefix}-victim.sh", _SLEEP_SCRIPT)
    victim_id = parse_job_id(
        cluster.sbatch([
            "-N1", "--exclusive", f"--nodelist={node}", "-q", qos, victim_script,
        ])
    )
    assert victim_id is not None, "victim submit failed"
    wait_job_state(cluster, victim_id, "R", timeout=30)
    _assert_scontrol_state(cluster, victim_id, "RUNNING", "victim initial")

    preempted_before = cluster.sdiag_jobs_preempted()

    aggressor_script = cluster.write_file(f"{prefix}-aggressor.sh", _QUICK_SCRIPT)
    aggressor_id = parse_job_id(
        cluster.sbatch([
            "-N1", "--exclusive", f"--nodelist={node}", "-q", qos, aggressor_script,
        ])
    )
    assert aggressor_id is not None, "aggressor submit failed"

    # Both jobs share a QOS, so nothing but the allow-list differs between this
    # test and its counterpart. The boost makes the raw job priorities differ too,
    # which must not change the outcome either way.
    cluster.scontrol("update", f"JobId={aggressor_id}", f"Priority={_AGGRESSOR_PRIORITY}")
    return victim_id, aggressor_id, preempted_before


class TestSameQosBlockedWithoutSelfListing:
    """A QOS that does not list itself must not preempt its own jobs, even when
    the pending job carries a far higher raw job priority."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_same_qos_cannot_preempt_without_self_listing(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        # Empty allow-list: this QOS may preempt nothing, including itself.
        c.sacctmgr(["add", "qos", "name=solo-burst", "priority=100", "preemptmode=cancel"])
        time.sleep(_CACHE_WARMUP_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_id, aggressor_id, preempted_before = _start_pair(
                c, "solo-burst", node, "same-qos-block"
            )

            wait_job_state(c, aggressor_id, "PD", timeout=30)

            # The same-QOS carve-out would permit this pairing; only the empty
            # allow-list stands in the way. Nothing must change over several
            # scheduler cycles.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "a QOS with an empty preempt allow-list must not evict its own job, "
                "even when the pending job's raw priority is far higher"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the boosted aggressor must keep waiting while the allow-list blocks it"
            )
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor after guard")
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision should have been made while the allow-list blocks it"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])


class TestSameQosAllowedWhenSelfListed:
    """A QOS that names itself in its own preempt allow-list may evict its own
    jobs, even though the two QOS priorities are necessarily equal.

    Same cluster config, same priority boost, same QOS priority as the blocked
    case above — the single difference is `preempt=solo-burst-open`.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_same_qos_preempts_when_self_listed(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        # Two steps, not one: CreateQos validates every allow-list name against
        # the QOS table before inserting the new row, so a QOS cannot name
        # itself at creation time ("QOS 'x' does not exist (in preempt
        # allow-list)"). It has to exist first, then be modified.
        c.sacctmgr(["add", "qos", "name=solo-burst-open", "priority=100",
                    "preemptmode=cancel"])
        c.sacctmgr(["modify", "qos", "name=solo-burst-open", "set",
                    "preempt=solo-burst-open"])
        time.sleep(_CACHE_WARMUP_SECS)

        listed = c.sacctmgr(["show", "qos", "format=Name,Preempt", "-P"])
        preempt_field = next(
            (line.split("|", 1)[1] for line in listed.splitlines()
             if line.startswith("solo-burst-open|")),
            None,
        )
        assert preempt_field is not None and "solo-burst-open" in preempt_field, (
            f"self-referential preempt allow-list was not stored in the Preempt field:\n{listed}"
        )

        victim_id = None
        aggressor_id = None
        try:
            victim_id, aggressor_id, preempted_before = _start_pair(
                c, "solo-burst-open", node, "same-qos-allow"
            )

            terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                "a QOS listing itself must be able to preempt its own jobs; "
                f"got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, victim_id, "CANCELLED", "victim after preemption")

            wait_job_state(c, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "RUNNING", "aggressor after preemption")
            assert c.sdiag_jobs_preempted() > preempted_before, (
                "a QOS listing itself must trigger a scheduler preemption decision"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])


class TestEmptyAllowListsDisablePreemptionClusterWide:
    """With preempt_type=qos_priority and every allow-list left empty, no job may
    preempt any other regardless of priority or QOS.

    A cluster whose preemption is misbehaving can be quieted either by clearing
    preempt_type outright or, if the gate must stay on for other QOS, by emptying
    the allow-lists. This test pins the second route: with the gate on, blank
    allow-lists are still a complete kill switch.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_empty_allow_lists_block_all_preemption(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        # A 100x QOS rank gap across two distinct QOS, neither listing the other.
        # Add `preempt=killswitch-low` to the high QOS and this pairing evicts.
        c.sacctmgr(["add", "qos", "name=killswitch-low", "priority=100", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=killswitch-high", "priority=10000", "preemptmode=cancel"])
        time.sleep(_CACHE_WARMUP_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("killswitch-victim.sh", _SLEEP_SCRIPT)
            victim_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-q", "killswitch-low", victim_script,
                ])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            aggressor_script = c.write_file("killswitch-aggressor.sh", _SLEEP_SCRIPT)
            aggressor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-q", "killswitch-high", aggressor_script,
                ])
            )
            assert aggressor_id is not None, "aggressor submit failed"
            wait_job_state(c, aggressor_id, "PD", timeout=30)

            preempted_before = c.sdiag_jobs_preempted()

            # Boost on top of the QOS gap so the priority threshold is
            # unambiguously cleared and only the allow-list can be responsible.
            c.scontrol("update", f"JobId={aggressor_id}", f"Priority={_AGGRESSOR_PRIORITY}")

            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "preempt_type=qos_priority with empty allow-lists must disable "
                "preemption entirely; the running job was evicted anyway"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the boosted high-QOS job must wait when no allow-list permits it"
            )
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision should have been made with all allow-lists empty"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
