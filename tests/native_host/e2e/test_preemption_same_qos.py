# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for preemption between two jobs in the *same* QOS.

Under preempt_type=qos_priority a pending job may only evict a running job when
the pending job's QOS lists the running job's QOS in its `preempt` allow-list.
Spur applies that check uniformly, with no special case for the two QOS being
identical — so a QOS naming *itself* is what enables same-QOS preemption.

This differs from Slurm, where the allow-list is consulted only on the
different-QOS branch and same-QOS preemption is gated by a dedicated
PreemptMode=WITHIN flag; there, self-listing is a no-op.

Every test here holds the priority gap constant and varies only the allow-list,
so a passing result cannot be explained by the priority threshold instead. That
matters: an existing test in test_burst_qos.py exercises two *different* burst
QOS with equal priority, where either gate alone would produce the same outcome.

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

# Large enough to clear the 2x effective-priority threshold against a job left
# at the default base priority, matching how test_preemption_modes.py boosts.
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
    it and boost the aggressor past the 2x preemption threshold.

    Returns (victim_id, aggressor_id, preempted_before) with the victim RUNNING
    and the aggressor PENDING, both asserted. preempted_before is the `Jobs
    preempted` counter sampled just before the boost, so callers can assert
    whether the scheduler actually acted on the allow-list gate.
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

    aggressor_script = cluster.write_file(f"{prefix}-aggressor.sh", _QUICK_SCRIPT)
    aggressor_id = parse_job_id(
        cluster.sbatch([
            "-N1", "--exclusive", f"--nodelist={node}", "-q", qos, aggressor_script,
        ])
    )
    assert aggressor_id is not None, "aggressor submit failed"
    wait_job_state(cluster, aggressor_id, "PD", timeout=30)
    _assert_scontrol_state(cluster, aggressor_id, "PENDING", "aggressor before boost")

    preempted_before = cluster.sdiag_jobs_preempted()

    # Both jobs share a QOS, so their base priorities are identical and no gap
    # exists until the aggressor is boosted. Boosting isolates the allow-list as
    # the only remaining variable between this test and its counterpart.
    cluster.scontrol("update", f"JobId={aggressor_id}", f"Priority={_AGGRESSOR_PRIORITY}")
    return victim_id, aggressor_id, preempted_before


class TestSameQosBlockedWithoutSelfListing:
    """A QOS that does not list itself must not preempt its own jobs, even when
    the pending job's priority is far above the 2x threshold."""

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

            # The priority gap is satisfied; only the allow-list stands in the
            # way. Nothing must change over several scheduler cycles.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "a QOS with an empty preempt allow-list must not evict its own job, "
                "even with a priority gap well past the 2x threshold"
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
    jobs, given the usual priority gap.

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

    This is the recommended stop-the-bleeding configuration for a cluster whose
    preemption is misbehaving, so it is worth an explicit test: flipping one
    config field turns already-blank allow-lists into an effective kill switch.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_empty_allow_lists_block_all_preemption(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        # A 100x QOS priority gap across two distinct QOS, neither listing the
        # other. Under preempt_type=none this pairing preempts readily.
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
