# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests proving fair-share has no say in preemption eligibility.

Fair-share used to feed the effective job priority that gated preemption:

    effective = base x min(fair_share, 10.0) x age_factor x max(partition_tier, 1)

Because the terms multiplied, fair-share (spanning roughly 33x in practice) could
overturn the QOS ordering outright (a QOS priority of 10000 against 100 spans only
10x on base). In production that let burst-QOS jobs at priority 100 repeatedly
cancel team-QOS jobs at priority 10000 whose owners had drifted over their target
share.

Eligibility is now decided by the QOS allow-list plus QOS rank alone. Fair-share
still orders pending jobs, but it can neither create nor block a preemption. These
tests assert both halves of that:

  FairShareCannotPreemptWithoutAllowList
      An extreme fair-share disparity favouring the aggressor evicts nothing while
      no allow-list permits it.

  QosRankDecidesRegardlessOfFairShare
      With an allow-list in place, QOS rank alone settles the outcome — a poor
      fair-share does not stop the higher-ranked QOS from preempting, and a
      stellar fair-share does not let the lower-ranked QOS preempt.

Every fixture here sets preempt_type=qos_priority; without it preemption is off
entirely and the negative tests would pass without exercising anything.

Requires:
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 60
_GUARD_SECS = 12

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
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = cluster.scontrol("show", "job", str(job_id))
    assert f"JobState={expected}" in show, (
        f"{label or 'job'} {job_id}: expected JobState={expected} in scontrol output:\n{show}"
    )


def _sql_str(value: str) -> str:
    """Escape a value for embedding as a single-quoted SQL string literal."""
    return value.replace("'", "''")


def _seed_usage(cluster, user: str, account: str, cpu_seconds: int) -> None:
    """Plant a decayed-usage row so fair-share has history to divide by.

    period_start is truncated to today (not NOW()) so exponential decay stays
    negligible while repeated calls for the same user/account still collide on
    the table's (user_name, account, period_start) primary key and update in
    place instead of inserting duplicate rows.
    """
    cluster.psql(
        "INSERT INTO usage "
        "(user_name, account, period_start, period_end, cpu_seconds, gpu_seconds, job_count) "
        f"VALUES ('{_sql_str(user)}', '{_sql_str(account)}', "
        f"date_trunc('day', NOW()), NOW(), {cpu_seconds}, 0, 1) "
        "ON CONFLICT (user_name, account, period_start) "
        "DO UPDATE SET cpu_seconds = EXCLUDED.cpu_seconds"
    )


def _setup_skewed_accounts(cluster, user: str, heavy: str, light: str) -> None:
    """Two accounts driven to opposite ends of the fair-share range.

    `heavy` gets the smaller weight and nearly all the recorded usage, so its
    fair-share factor sinks; `light` gets the larger weight and effectively no
    usage, so its factor pins to the 10.0 cap. That is the ~33x disparity that
    used to be enough to overturn a 100x QOS priority ordering.
    """
    cluster.sacctmgr(["add", "account", f"name={heavy}", "fairshare=1"])
    cluster.sacctmgr(["add", "account", f"name={light}", "fairshare=10"])
    cluster.sacctmgr(["add", "user", f"name={user}", f"account={heavy}"])
    cluster.sacctmgr(["add", "user", f"name={user}", f"account={light}"])
    _seed_usage(cluster, user, heavy, _HEAVY_USAGE)
    _seed_usage(cluster, user, light, _LIGHT_USAGE)


class TestFairShareCannotPreemptWithoutAllowList:
    """No fair-share disparity, however extreme, may evict a job that no QOS
    allow-list permits preempting.

    This is the direct regression test for the production defect: a burst QOS at
    priority 100 cancelling team-QOS jobs at priority 10000 because the team's
    owners had drifted over their fair-share target. The setup below reproduces
    that disparity exactly — the aggressor holds the maximum fair-share factor and
    the victim the minimum — and asserts that nothing happens, because the
    aggressor's QOS names nothing in its preempt allow-list.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_extreme_fairshare_disparity_evicts_nothing(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        _setup_skewed_accounts(c, user, "fs-heavy", "fs-light")

        # The victim also outranks the aggressor by 100x on QOS priority, so
        # fair-share is the only thing that could possibly favour the aggressor.
        # Neither QOS lists the other.
        c.sacctmgr(["add", "qos", "name=fs-team", "priority=10000", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=fs-burst", "priority=100", "preemptmode=cancel"])
        time.sleep(_FAIRSHARE_REFRESH_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("fs-victim.sh", _SLEEP_SCRIPT)
            victim_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-heavy", "-q", "fs-team", victim_script,
                ])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            # Sample the counter before the aggressor exists: the scheduler runs
            # on a sub-second cycle and can preempt while the submit call is
            # still returning, so a baseline taken any later may already include
            # the preemption this test is trying to catch.
            preempted_before = c.sdiag_jobs_preempted()

            aggressor_script = c.write_file("fs-aggressor.sh", _SLEEP_SCRIPT)
            aggressor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-light", "-q", "fs-burst", aggressor_script,
                ])
            )
            assert aggressor_id is not None, "aggressor submit failed"
            wait_job_state(c, aggressor_id, "PD", timeout=30)

            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "a fair-share advantage must not evict anything on its own; in "
                "production this exact disparity let a QOS priority of 100 cancel "
                "jobs at QOS priority 10000"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the fair-share-favoured job must wait rather than displace the "
                "higher-ranked QOS"
            )
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor after guard")
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision may be recorded when no allow-list permits one"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])


class TestQosRankDecidesRegardlessOfFairShare:
    """With an allow-list in place, QOS rank settles the outcome and fair-share
    cannot override it in either direction.

    The two tests are mirror images: the same accounts and the same ~33x
    fair-share disparity, with only the direction of the QOS rank swapped. If
    fair-share still leaked into the eligibility decision, exactly one of them
    would fail.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_higher_qos_rank_preempts_despite_worse_fairshare(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        _setup_skewed_accounts(c, user, "fs-a-heavy", "fs-a-light")

        # The aggressor outranks the victim on QOS but runs from the account with
        # the exhausted fair-share; the victim sits on the pristine one.
        c.sacctmgr(["add", "qos", "name=fs-a-victim", "priority=100",
                    "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=fs-a-hunter", "priority=10000",
                    "preempt=fs-a-victim"])
        time.sleep(_FAIRSHARE_REFRESH_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("fs-a-victim.sh", _SLEEP_SCRIPT)
            victim_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-a-light", "-q", "fs-a-victim", victim_script,
                ])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            preempted_before = c.sdiag_jobs_preempted()

            aggressor_script = c.write_file("fs-a-hunter.sh", _QUICK_SCRIPT)
            aggressor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-a-heavy", "-q", "fs-a-hunter", aggressor_script,
                ])
            )
            assert aggressor_id is not None, "aggressor submit failed"

            terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                "an allow-listed, higher-ranked QOS must preempt even when its "
                f"owner has exhausted their fair-share; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, victim_id, "CANCELLED", "victim after preemption")

            # preempt_mode=cancel lands the victim in CANCELLED, which is also
            # where an ordinary scancel would leave it. The scheduler's own
            # counter is what distinguishes a preemption from any other
            # termination, so assert the decision, not just the end state.
            assert c.sdiag_jobs_preempted() > preempted_before, (
                "victim reached a terminal state but the scheduler recorded no "
                "preemption; it died for some other reason and this test would "
                "otherwise pass for the wrong reason"
            )

            wait_job_state(c, aggressor_id, "R", timeout=30)
            _assert_scontrol_state(c, aggressor_id, "RUNNING", "aggressor after preemption")
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])

    def test_lower_qos_rank_cannot_preempt_despite_better_fairshare(
        self, accounting_cluster
    ):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        _setup_skewed_accounts(c, user, "fs-b-heavy", "fs-b-light")

        # Mirror of the test above: the aggressor now holds the pristine
        # fair-share but is outranked on QOS. The allow-list still names the
        # victim, so the strict rank comparison is the only thing left to block
        # eviction — and fair-share must not be able to substitute for it.
        c.sacctmgr(["add", "qos", "name=fs-b-victim", "priority=10000",
                    "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=fs-b-hunter", "priority=100",
                    "preempt=fs-b-victim"])
        time.sleep(_FAIRSHARE_REFRESH_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("fs-b-victim.sh", _SLEEP_SCRIPT)
            victim_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-b-heavy", "-q", "fs-b-victim", victim_script,
                ])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            preempted_before = c.sdiag_jobs_preempted()

            aggressor_script = c.write_file("fs-b-hunter.sh", _SLEEP_SCRIPT)
            aggressor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-b-light", "-q", "fs-b-hunter", aggressor_script,
                ])
            )
            assert aggressor_id is not None, "aggressor submit failed"
            wait_job_state(c, aggressor_id, "PD", timeout=30)

            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "a lower-ranked QOS must not evict a higher-ranked one even with "
                "an allow-list entry and a maximal fair-share advantage"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the lower-ranked aggressor must wait its turn"
            )
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor after guard")
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no preemption decision may be recorded when the aggressor's QOS "
                "does not outrank the victim's"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
