# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for fair-share's influence on preemption eligibility.

Preemption fires on a gap in *effective* priority, and effective priority is a
product:

    effective = base x min(fair_share, 10.0) x age_factor x max(partition_tier, 1)
    base      = explicit --priority, else 1000 + qos.priority

Because the terms multiply, fair-share does not merely break ties between jobs
of equal QOS — it can overturn the QOS ordering outright. Fair-share spans
roughly 33x in practice (capped at 10.0 above, sinking toward the account's
target share below) while a QOS priority of 10000 against 100 spans only 10x
(base 11000 against 1100). The wider range wins.

These tests pin that behaviour down so a change to the priority model is a
deliberate decision rather than an accident.

Requires:
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)

Note the fixtures deliberately leave scheduler.preempt_type unset, so it
defaults to `none` and the per-QOS `preempt` allow-list is not consulted. That
is the configuration under test: eligibility rests entirely on the priority gap.
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
# slack before the scheduler is guaranteed to see it.
_FAIRSHARE_REFRESH_SECS = 30

# Required when the test runner SSHes in as root: spurd refuses to execute jobs
# as uid 0 unless this is explicitly enabled.
_AUTH_ROOT = {"auth": {"plugin": "none", "allow_root_jobs": True}}

_CANCEL_PARTITION = {
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
}


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


class TestFairShareOverturnsQosPriority:
    """A QOS priority of 100 must not be able to evict a QOS priority of 10000 —
    yet it does, because fair-share is multiplied into the same number that gates
    preemption.

    Reproduces a production pattern where burst-QOS jobs (priority 100)
    repeatedly cancelled team-QOS jobs (priority 10000) whose owners had drifted
    over their fair-share target.

    Arithmetic, with both accounts on the default partition (tier 1) and both
    jobs freshly submitted (age_factor ~1.0):

      target_share(heavy) = 1 / 11  = 0.0909      (fairshare weight 1 of 11)
      target_share(light) = 10 / 11 = 0.909       (fairshare weight 10 of 11)

      heavy holds ~all recorded usage  -> actual ~1.0  -> fs ~0.0909
      light holds ~none                -> actual clamped to the 0.001 epsilon
                                       -> fs 909, capped to 100, then to 10.0

      victim  (team,  base 11000) effective ~ 11000 x 0.0909 =  1000
      pending (burst, base  1100) effective ~  1100 x 10.0   = 11000

      preemption fires when victim < pending / 2, i.e. 1000 < 5500. True, with
      a 5.5x margin so the assertion does not sit on the threshold.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_CANCEL_PARTITION, **_AUTH_ROOT}

    def test_low_qos_priority_preempts_high_qos_priority_via_fairshare(
        self, accounting_cluster
    ):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        # Unequal fair-share weights give the two accounts very different
        # targets; planted usage then drives their actual shares apart.
        c.sacctmgr(["add", "account", "name=fs-heavy", "fairshare=1"])
        c.sacctmgr(["add", "account", "name=fs-light", "fairshare=10"])
        c.sacctmgr(["add", "user", f"name={user}", "account=fs-heavy"])
        c.sacctmgr(["add", "user", f"name={user}", "account=fs-light"])

        # The victim outranks the aggressor by 100x on QOS priority alone.
        c.sacctmgr(["add", "qos", "name=fs-team", "priority=10000", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=fs-burst", "priority=100", "preemptmode=cancel"])

        _seed_usage(c, user, "fs-heavy", 999_000_000)
        _seed_usage(c, user, "fs-light", 1)
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
            # the preemption this test is trying to observe.
            preempted_before = c.sdiag_jobs_preempted()

            aggressor_script = c.write_file("fs-aggressor.sh", _QUICK_SCRIPT)
            aggressor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-light", "-q", "fs-burst", aggressor_script,
                ])
            )
            assert aggressor_id is not None, "aggressor submit failed"

            terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                "a QOS priority of 100 evicted a QOS priority of 10000 in production; "
                "this test asserts that behaviour still reproduces, so that a change to "
                f"the priority model is caught here. got {terminal!r}"
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


class TestEqualFairShareLeavesQosPriorityIntact:
    """The control for the test above: with fair-share neutral on both sides, the
    QOS priority ordering holds and the low-priority job cannot evict anyone.

    Without this, the inversion test could pass for the wrong reason — a bug that
    let *any* pending job preempt would satisfy it just as well.
    """

    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_CANCEL_PARTITION, **_AUTH_ROOT}

    def test_low_qos_cannot_preempt_high_qos_without_fairshare_divergence(
        self, accounting_cluster
    ):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        # Identical weights and identical planted usage: both accounts land on
        # the same fair-share factor, so only QOS priority separates the jobs.
        c.sacctmgr(["add", "account", "name=fs-even-a", "fairshare=1"])
        c.sacctmgr(["add", "account", "name=fs-even-b", "fairshare=1"])
        c.sacctmgr(["add", "user", f"name={user}", "account=fs-even-a"])
        c.sacctmgr(["add", "user", f"name={user}", "account=fs-even-b"])

        c.sacctmgr(["add", "qos", "name=fs-even-team", "priority=10000", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=fs-even-burst", "priority=100", "preemptmode=cancel"])

        _seed_usage(c, user, "fs-even-a", 1_000_000)
        _seed_usage(c, user, "fs-even-b", 1_000_000)
        time.sleep(_FAIRSHARE_REFRESH_SECS)

        victim_id = None
        aggressor_id = None
        try:
            victim_script = c.write_file("fs-even-victim.sh", _SLEEP_SCRIPT)
            victim_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-even-a", "-q", "fs-even-team", victim_script,
                ])
            )
            assert victim_id is not None, "victim submit failed"
            wait_job_state(c, victim_id, "R", timeout=30)
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

            # Baseline before the aggressor exists, matching the inversion test:
            # a later sample could absorb the very preemption being guarded
            # against and turn this assertion into a no-op.
            preempted_before = c.sdiag_jobs_preempted()

            aggressor_script = c.write_file("fs-even-aggressor.sh", _SLEEP_SCRIPT)
            aggressor_id = parse_job_id(
                c.sbatch([
                    "-N1", "--exclusive", f"--nodelist={node}",
                    "-A", "fs-even-b", "-q", "fs-even-burst", aggressor_script,
                ])
            )
            assert aggressor_id is not None, "aggressor submit failed"
            wait_job_state(c, aggressor_id, "PD", timeout=30)

            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "a QOS priority of 100 must not evict a QOS priority of 10000 when "
                "fair-share is neutral on both sides"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the low-QOS job must wait rather than displace the high-QOS job"
            )
            _assert_scontrol_state(c, aggressor_id, "PENDING", "aggressor after guard")
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no preemption may be recorded while fair-share is neutral"
            )
        finally:
            for jid in (victim_id, aggressor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
