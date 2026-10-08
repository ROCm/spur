# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests that preemption only ever fires when it helps.

A pending job that no node could host even when completely idle must evict
nobody: every eviction it triggers destroys work and still leaves it pending,
so it would eat the cluster one victim per scheduler cycle forever. The classic
way to land in that state is an unsatisfiable generic GRES (``--gres 1`` parses
as a resource named "1" that no node declares), which is invisible in
``ReqTRES`` and otherwise reports the same ``Resources`` as a job legitimately
queued behind others.

The multi-node cases cover the other half: a job needing several nodes must
evict the whole victim set or none of it. A partial eviction is strictly worse
than doing nothing.

Each "nothing happens" assertion is paired with a positive control on the same
configuration, so a green result cannot come from the fixture being unable to
preempt at all.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 60
_WAIT_RUN = 60
# Several scheduler cycles, so "nothing happened" means the scheduler looked
# and declined rather than not having run yet.
_GUARD_SECS = 15

# Clears the 2x effective-priority threshold against a job left at the default
# base priority, matching how the other preemption suites boost.
_AGGRESSOR_PRIORITY = 1_000_000

# Required when the test runner SSHes in as root: spurd refuses to execute jobs
# as uid 0 unless this is explicitly enabled.
_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}

# A resource no node declares, so no eviction can ever make the job placeable.
_BOGUS_GRES = "--gres=1"

_SINGLE_PARTITION_CONFIG = {
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
    # Explicit zero so a victim is preemptable the moment it starts and the
    # exempt window can never be mistaken for the reason nothing was evicted.
    # qos_priority is required: without it try_preempt never runs at all.
    "scheduler": {"preempt_exempt_time": 0, "preempt_type": "qos_priority"},
    **_AUTH_ROOT,
}

# Self-listing QOS shared by victim and aggressor: qos_priority requires an
# explicit allow-list, and same-name eligibility falls back to submit order,
# which every test here already satisfies (victim submitted first).
_QOS = "satisfiability"


def _ensure_satisfiability_qos(cluster) -> None:
    # A QOS can't list itself at creation time (the name doesn't exist yet),
    # so add it bare first, then modify it to self-list.
    cluster.sacctmgr(["add", "qos", f"name={_QOS}"])
    cluster.sacctmgr(["modify", "qos", f"name={_QOS}", "set", f"preempt={_QOS}"])
    time.sleep(15)  # past the QoS cache refresh floor


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = cluster.scontrol("show", "job", str(job_id))
    assert f"JobState={expected}" in show, (
        f"{label or 'job'} {job_id}: expected JobState={expected} in scontrol output:\n{show}"
    )


def _run_victim(cluster, node: str, prefix: str, extra: list[str] | None = None) -> int:
    """Submit an exclusive sleeper pinned to *node* and wait for it to run."""
    script = cluster.write_file(f"{prefix}.sh", _SLEEP_SCRIPT)
    args = ["-J", prefix, "-N1", "--exclusive", f"--nodelist={node}"]
    args += extra or []
    job_id = parse_job_id(cluster.sbatch(args + [script]))
    assert job_id is not None, f"{prefix} submit failed"
    wait_job_state(cluster, job_id, "R", timeout=_WAIT_RUN)
    _assert_scontrol_state(cluster, job_id, "RUNNING", prefix)
    return job_id


def _queue_aggressor(cluster, prefix: str, extra: list[str]) -> int:
    """Submit a pending aggressor and boost it past the 2x priority threshold."""
    script = cluster.write_file(f"{prefix}.sh", _QUICK_SCRIPT)
    job_id = parse_job_id(cluster.sbatch(["-J", prefix] + extra + [script]))
    assert job_id is not None, f"{prefix} submit failed"
    wait_job_state(cluster, job_id, "PD", timeout=30)
    cluster.scontrol("update", f"JobId={job_id}", f"Priority={_AGGRESSOR_PRIORITY}")
    return job_id


def _scancel_all(cluster, job_ids) -> None:
    for job_id in job_ids:
        if job_id is not None:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestUnplaceableAggressorEvictsNothing:
    """A top-priority job that no node can ever host must not preempt anyone."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _SINGLE_PARTITION_CONFIG

    def test_unsatisfiable_gres_aggressor_preempts_nobody(self, accounting_cluster):
        """End-to-end outcome for the production shape. The structural gate is
        what keeps this job out of try_preempt; the victim-set proof is the
        backstop, covered on its own by the multi-node cases below."""
        c = accounting_cluster
        _ensure_satisfiability_qos(c)
        node = c.node_names[0]
        victim_id = aggressor_id = None
        try:
            victim_id = _run_victim(c, node, "unsat-victim", extra=["-q", _QOS])
            preempted_before = c.sdiag_jobs_preempted()
            aggressor_id = _queue_aggressor(
                c,
                "unsat-aggressor",
                ["-N1", "--exclusive", f"--nodelist={node}", "-q", _QOS, _BOGUS_GRES],
            )

            time.sleep(_GUARD_SECS)

            sq = c.squeue_all()
            assert job_state(sq, victim_id) == "R", (
                "a job no node can host must evict nobody, however high its priority"
            )
            _assert_scontrol_state(c, victim_id, "RUNNING", "victim after guard")
            assert job_state(sq, aggressor_id) == "PD", (
                "the unplaceable aggressor must stay pending"
            )
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no job should have been preempted on behalf of an unplaceable job"
            )
        finally:
            _scancel_all(c, [victim_id, aggressor_id])

    def test_unplaceable_job_reports_a_real_reason(self, cluster):
        """The user must see why, not the generic Resources every queued job shows."""
        c = cluster
        job_id = None
        try:
            script = c.write_file("unsat-reason.sh", _QUICK_SCRIPT)
            job_id = parse_job_id(c.sbatch(["-J", "unsat-reason", "-N1", _BOGUS_GRES, script]))
            assert job_id is not None, "submit failed"
            wait_job_state(c, job_id, "PD", timeout=30)

            deadline = time.time() + 30
            show = ""
            while time.time() < deadline:
                show = c.scontrol("show", "job", str(job_id))
                if "Requested node configuration is not available" in show:
                    break
                time.sleep(2)
            assert "Requested node configuration is not available" in show, (
                f"a structurally unplaceable job must report why:\n{show}"
            )
        finally:
            _scancel_all(c, [job_id])

    def test_placeable_aggressor_still_preempts(self, accounting_cluster):
        """Control: the same submission minus the unsatisfiable gres does evict."""
        c = accounting_cluster
        _ensure_satisfiability_qos(c)
        node = c.node_names[0]
        victim_id = aggressor_id = None
        try:
            victim_id = _run_victim(c, node, "ctrl-victim", extra=["-q", _QOS])
            aggressor_id = _queue_aggressor(
                c, "ctrl-aggressor", ["-N1", "--exclusive", f"--nodelist={node}", "-q", _QOS]
            )

            terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"a placeable aggressor must still preempt; victim ended {terminal!r}"
            )
            wait_job_state(c, aggressor_id, "R", timeout=_WAIT_RUN)
        finally:
            _scancel_all(c, [victim_id, aggressor_id])


class TestMultiNodeAggressorEvictsAllOrNothing:
    """A job needing two nodes takes the full victim set or leaves both alone."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _SINGLE_PARTITION_CONFIG

    def test_partial_victim_set_evicts_nobody(self, accounting_cluster):
        """Covers the PreemptMode=Off eligibility gate, excluded before the
        satisfiability proof runs; see _via_satisfiability_proof for that proof."""
        c = accounting_cluster
        if len(c.node_names) < 2:
            pytest.skip("requires 2 nodes")
        _ensure_satisfiability_qos(c)
        first, second = c.node_names[0], c.node_names[1]
        # Overlays the second node. PreemptMode defaults to OFF on create, so a
        # job submitted here is ineligible for eviction but the node is not.
        c.scontrol(
            "create-partition",
            "--name=shielded",
            f"--nodes={second}",
            "--max-time=24:00:00",
            "--default-time=10:00",
        )
        evictable_id = shielded_id = aggressor_id = None
        try:
            evictable_id = _run_victim(c, first, "partial-evictable", extra=["-q", _QOS])
            shielded_id = _run_victim(
                c, second, "partial-shielded", extra=["-p", "shielded", "-q", _QOS]
            )
            preempted_before = c.sdiag_jobs_preempted()
            # Pinned to exactly these two nodes so a bed with spare capacity
            # elsewhere cannot place the aggressor without preempting.
            aggressor_id = _queue_aggressor(
                c,
                "partial-aggressor",
                ["-N2", "--exclusive", "-p", "default", "-q", _QOS, f"--nodelist={first},{second}"],
            )

            time.sleep(_GUARD_SECS)

            sq = c.squeue_all()
            assert job_state(sq, evictable_id) == "R", (
                "evicting half a victim set destroys work without placing the "
                "aggressor, so the evictable victim must be left alone"
            )
            assert job_state(sq, shielded_id) == "R", (
                "a job in a PreemptMode=off partition must never be evicted"
            )
            assert job_state(sq, aggressor_id) == "PD", (
                "the aggressor cannot be placed, so it must stay pending"
            )
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no partial eviction should have been recorded"
            )
        finally:
            _scancel_all(c, [evictable_id, shielded_id, aggressor_id])
            c.cli_allow_fail(["scontrol", "delete-partition", "--name=shielded"])

    def test_complete_victim_set_is_evicted_together(self, accounting_cluster):
        """Control: with both victims evictable, both go and the aggressor runs."""
        c = accounting_cluster
        if len(c.node_names) < 2:
            pytest.skip("requires 2 nodes")
        _ensure_satisfiability_qos(c)
        first, second = c.node_names[0], c.node_names[1]
        victim_ids = []
        aggressor_id = None
        try:
            for i, node in enumerate((first, second)):
                victim_ids.append(_run_victim(c, node, f"full-victim-{i}", extra=["-q", _QOS]))
            aggressor_id = _queue_aggressor(
                c,
                "full-aggressor",
                ["-N2", "--exclusive", "-p", "default", "-q", _QOS, f"--nodelist={first},{second}"],
            )

            for victim_id in victim_ids:
                terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
                assert terminal in ("CA", "GONE"), (
                    f"every victim in the set must be evicted; {victim_id} ended {terminal!r}"
                )
            wait_job_state(c, aggressor_id, "R", timeout=_WAIT_RUN)
            show = c.scontrol("show", "job", str(aggressor_id))
            assert "NumNodes=2" in show, (
                f"the aggressor must land on both freed nodes; got:\n{show}"
            )
        finally:
            _scancel_all(c, victim_ids + [aggressor_id])

    def test_partial_victim_set_evicts_nobody_via_satisfiability_proof(self, accounting_cluster):
        """Both victims clear PreemptMode=cancel eligibility; only
        satisfiable_victim_set's all-or-nothing proof decides the outcome."""
        c = accounting_cluster
        if len(c.node_names) < 2:
            pytest.skip("requires 2 nodes")
        first, second = c.node_names[0], c.node_names[1]

        # exempt-shield outranked by _QOS so only its exempt-time guard, not the
        # allow-list/rank gate, is what the aggressor has left to fail against.
        c.sacctmgr(["add", "qos", "name=exempt-shield", "preemptexempttime=3600"])
        c.sacctmgr(["add", "qos", f"name={_QOS}", "priority=10"])
        c.sacctmgr(["modify", "qos", f"name={_QOS}", "set", f"preempt={_QOS},exempt-shield"])
        time.sleep(15)  # past the QoS cache refresh floor

        evictable_id = shielded_id = aggressor_id = None
        try:
            evictable_id = _run_victim(c, first, "proof-evictable", extra=["-q", _QOS])
            shielded_id = _run_victim(
                c, second, "proof-shielded", extra=["-q", "exempt-shield"]
            )
            preempted_before = c.sdiag_jobs_preempted()
            aggressor_id = _queue_aggressor(
                c,
                "proof-aggressor",
                ["-N2", "--exclusive", "-p", "default", "-q", _QOS, f"--nodelist={first},{second}"],
            )

            time.sleep(_GUARD_SECS)

            sq = c.squeue_all()
            assert job_state(sq, evictable_id) == "R", (
                "both victims clear PreemptMode=cancel; only the satisfiability "
                "proof's all-or-nothing refusal leaves the evictable one alone"
            )
            assert job_state(sq, shielded_id) == "R", (
                "the exempt-time guard must still hold"
            )
            assert job_state(sq, aggressor_id) == "PD", (
                "the aggressor cannot be placed, so it must stay pending"
            )
            assert c.sdiag_jobs_preempted() == preempted_before, (
                "no partial eviction should have been recorded"
            )
        finally:
            _scancel_all(c, [evictable_id, shielded_id, aggressor_id])
