# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for the QoS group-node (``GrpTRES=node``) cap.

Two complementary experiments, both driven purely by QoS configuration — no
product code is changed:

  Scenario A (cap holds against a wave): a QoS capped at ``node=cap`` with one
  job already RUNNING and a wave of further single-node exclusive jobs must
  never place more than ``cap`` distinct nodes, even when free nodes exist. The
  jobs over the cap pend on ``QOSGrpNodeLimit``.

  Scenario B (cap lowered after admission): a QoS is first capped high enough to
  admit every job, so all run; then the cap is lowered below the running count
  with ``sacctmgr modify``. The already-running jobs are never re-checked, so
  they stay up and the QoS reads ``GrpTRES=node=cap(usage)`` with
  ``OverLimit=GrpTRES`` while new jobs correctly block on ``QOSGrpNodeLimit``.
  This shows the "six on a node=3 cap" observation is reproducible by lowering a
  cap after the wave, with no admission-path defect.

Requires:
  - Postgres on node 0 (accounting_cluster fixture; skips when Docker is absent)
  - at least 2 nodes in SPUR_TEST_NODES (to prove the cap blocks while a free
    node is available)
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"

_QOS_SETTLE = 15
_GUARD_SECS = 12

_BASE_CONFIG = {
    "partitions": [
        {
            "name": "default",
            "state": "UP",
            "default": True,
            "nodes": "ALL",
            "max_time": "24:00:00",
            "default_time": "10:00",
        }
    ],
    "auth": {"plugin": "none", "allow_root_jobs": True},
}


def _job_reason(cluster, job_id: int) -> str:
    """The scheduler Reason string from ``scontrol show job``."""
    show = cluster.scontrol("show", "job", str(job_id))
    for token in show.split():
        if token.startswith("Reason="):
            return token[len("Reason=") :]
    return ""


def _running_count(cluster, job_ids: list[int]) -> int:
    sq = cluster.squeue_all()
    return sum(1 for jid in job_ids if job_state(sq, jid) == "R")


def _submit_exclusive(cluster, qos: str, tag: str) -> int:
    script = cluster.write_file(f"{tag}.sh", _SLEEP_SCRIPT)
    jid = parse_job_id(
        cluster.sbatch_when_qos_ready(["-N1", "--exclusive", "-q", qos, script])
    )
    assert jid is not None, f"submit failed for {tag}"
    return jid


class TestGrpNodeCapHoldsAgainstWave:
    """A ``node=cap`` QoS with one job already running plus a wave of exclusive
    single-node jobs must admit at most ``cap`` distinct nodes — the surplus
    pends on QOSGrpNodeLimit even though free nodes remain."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_grp_node_cap_not_oversubscribed_by_a_wave(self, accounting_cluster):
        c = accounting_cluster
        n = len(c.node_names)
        if n < 2:
            pytest.skip("grp-node cap test needs >= 2 nodes to prove a free node stays unused")
        cap = n - 1  # leave exactly one node that the cap must keep idle

        c.sacctmgr(["add", "qos", "name=capped", f"grptres=node={cap}"])
        time.sleep(_QOS_SETTLE)

        ids: list[int] = []
        try:
            # One job already RUNNING, mirroring the reported pre-existing job.
            first = _submit_exclusive(c, "capped", "capped-running")
            ids.append(first)
            wait_job_state(c, first, "R", timeout=45)

            # The wave: fill the remaining node slots plus one over the cap.
            for i in range(n - 1):
                ids.append(_submit_exclusive(c, "capped", f"capped-wave{i}"))

            # Let the scheduler reach steady state.
            time.sleep(_GUARD_SECS)

            running = _running_count(c, ids)
            assert running == cap, (
                f"grp node cap={cap} must bound concurrent nodes, but {running} "
                f"jobs are RUNNING\n{c.squeue_all()}"
            )

            sq = c.squeue_all()
            pend = [jid for jid in ids if job_state(sq, jid) == "PD"]
            assert pend, "at least one wave job must be pending at the cap"
            blocked = [jid for jid in pend if _job_reason(c, jid) == "QOSGrpNodeLimit"]
            assert blocked, (
                "a job over the cap must report Reason=QOSGrpNodeLimit; "
                f"pending reasons: {[(jid, _job_reason(c, jid)) for jid in pend]}"
            )

            # The assoc-mgr view must agree: usage equals the cap, not more.
            assoc = c.scontrol("show", "assoc_mgr")
            assert f"node={cap}({cap})" in assoc, (
                f"assoc_mgr must show GrpTRES node={cap}({cap}); got:\n{assoc}"
            )
        finally:
            for jid in ids:
                c.cli_allow_fail(["scancel", str(jid)])


class TestGrpNodeCapLoweredAfterAdmission:
    """Lowering a QoS node cap below its running usage does not evict the
    already-running jobs (they are never re-checked); it only blocks new ones.
    The QoS then reads OverLimit=GrpTRES — reproducing the reported
    node=cap(usage>cap) observation with no code change."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_lowering_cap_keeps_running_jobs_and_blocks_new(self, accounting_cluster):
        c = accounting_cluster
        n = len(c.node_names)
        if n < 2:
            pytest.skip("cap-lowering test needs >= 2 nodes")
        lowered = n - 1  # new cap, strictly below the running count n

        # Start high enough that every node runs a job.
        c.sacctmgr(["add", "qos", "name=elastic", f"grptres=node={n}"])
        time.sleep(_QOS_SETTLE)

        ids: list[int] = []
        try:
            for i in range(n):
                jid = _submit_exclusive(c, "elastic", f"elastic{i}")
                ids.append(jid)
            for jid in ids:
                wait_job_state(c, jid, "R", timeout=60)
            assert _running_count(c, ids) == n, "all jobs must run under the high cap"

            # Lower the cap below the running count. Running jobs are not re-checked.
            c.sacctmgr(["modify", "qos", "name=elastic", f"grptres=node={lowered}"])
            time.sleep(_QOS_SETTLE)

            assert _running_count(c, ids) == n, (
                "lowering the cap must NOT evict already-running jobs; they are "
                f"never re-checked\n{c.squeue_all()}"
            )

            assoc = c.scontrol("show", "assoc_mgr")
            assert f"node={lowered}({n})" in assoc and "OverLimit=GrpTRES" in assoc, (
                f"assoc_mgr must show GrpTRES node={lowered}({n}) OverLimit=GrpTRES; "
                f"got:\n{assoc}"
            )

            # A new job now correctly blocks on the lowered cap.
            newcomer = _submit_exclusive(c, "elastic", "elastic-new")
            ids.append(newcomer)
            time.sleep(_GUARD_SECS)
            assert job_state(c.squeue_all(), newcomer) == "PD", (
                "a job submitted after the cap was lowered must pend"
            )
            assert _job_reason(c, newcomer) == "QOSGrpNodeLimit", (
                f"new job must block on QOSGrpNodeLimit; got {_job_reason(c, newcomer)!r}"
            )
        finally:
            for jid in ids:
                c.cli_allow_fail(["scancel", str(jid)])
