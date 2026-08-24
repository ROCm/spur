# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for the burst-QoS pattern.

Burst QoS is a convention, not a dedicated field: it is a QoS with a negative
priority delta that is listed in the normal QoS's preempt allow-list.  Jobs
under burst QoS fill overflow capacity opportunistically but yield immediately
when a normal-priority job needs the slot.

Requires:
  - preempt_type=qos_priority (scheduler config) so allow-list gating applies
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)

Every state transition is verified through both squeue and scontrol show job.
Every interim state is asserted explicitly so that a bug at any stage is caught
rather than silently masked by a later assertion.

NOTE: REST API cross-verification is not yet covered — the e2e infrastructure
has no REST client helper. Tracked as a separate gap.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT  = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 60
_WAIT_RESUME  = 90
_GUARD_SECS   = 12

# Every cluster in this file uses the same base config:
# preempt_type=qos_priority so the QoS allow-list is enforced, and
# allow_root_jobs so the tests work when the runner SSHes in as root.
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
    "auth": {"plugin": "none", "allow_root_jobs": True},
}


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    show = cluster.scontrol("show", "job", str(job_id))
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


class TestBurstQosPreemptedByNormal:
    """A burst job running on the only node must be cancelled when a normal job
    arrives, because:
      [1] normal QoS has burst in its preempt allow-list → preemption authorised
      [2] burst has a deeply negative priority delta → gap exceeds the 2× threshold
    Both conditions must hold simultaneously for preemption to fire."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_burst_qos_preempted_by_normal_qos_via_priority(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=burst",  "priority=-5000"])
        # normal lists burst in its allow-list → authorised to preempt burst jobs.
        c.sacctmgr(["add", "qos", "name=normal", "priority=0", "preempt=burst"])
        time.sleep(15)

        burst_id = None
        normal_id = None
        try:
            burst_script = c.write_file("burst-victim.sh", _SLEEP_SCRIPT)
            burst_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "burst", burst_script])
            )
            assert burst_id is not None, "burst submit failed"
            wait_job_state(c, burst_id, "R", timeout=30)
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst initial")

            normal_script = c.write_file("normal-aggressor.sh", _QUICK_SCRIPT)
            normal_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "normal", normal_script])
            )
            assert normal_id is not None, "normal submit failed"
            wait_job_state(c, normal_id, "PD", timeout=30)
            _assert_scontrol_state(c, normal_id, "PENDING", "normal before preemption")

            # Burst job must be cancelled — allow-list permits it, priority gap qualifies.
            terminal = wait_job(c, burst_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"burst job must be preempted by normal job; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, burst_id, "CANCELLED", "burst after preemption")

            # Normal job must take the freed slot and run.
            wait_job_state(c, normal_id, "R", timeout=30)
            _assert_scontrol_state(c, normal_id, "RUNNING", "normal after preemption")

            final = wait_job(c, normal_id, timeout=30)
            assert final == "CD", f"normal job must complete successfully; got {final!r}"
        finally:
            if burst_id is not None:
                c.cli_allow_fail(["scancel", str(burst_id)])
            if normal_id is not None:
                c.cli_allow_fail(["scancel", str(normal_id)])


class TestBurstQosNotPreemptedByAnotherBurst:
    """Two burst jobs competing for the same node: neither must preempt the other.
    Both have the same low priority, so the running burst job is not above the 2×
    threshold that would qualify the pending one for preemption."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_burst_qos_not_preempted_by_another_burst_qos_job(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        # Two burst QoSes with identical low priority; neither has the other
        # in its allow-list, so even a modest priority gap would be blocked.
        c.sacctmgr(["add", "qos", "name=burst-a", "priority=-5000"])
        c.sacctmgr(["add", "qos", "name=burst-b", "priority=-5000"])
        time.sleep(15)

        burst_a_id = None
        burst_b_id = None
        try:
            script_a = c.write_file("burst-a.sh", _SLEEP_SCRIPT)
            burst_a_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "burst-a", script_a])
            )
            assert burst_a_id is not None, "burst-a submit failed"
            wait_job_state(c, burst_a_id, "R", timeout=30)
            _assert_scontrol_state(c, burst_a_id, "RUNNING", "burst-a initial")

            script_b = c.write_file("burst-b.sh", _SLEEP_SCRIPT)
            burst_b_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "burst-b", script_b])
            )
            assert burst_b_id is not None, "burst-b submit failed"
            wait_job_state(c, burst_b_id, "PD", timeout=30)
            _assert_scontrol_state(c, burst_b_id, "PENDING", "burst-b before guard")

            # Neither condition for preemption is met: no allow-list entry AND
            # equal priority. After several scheduler cycles nothing must change.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, burst_a_id) == "R", (
                "running burst job must not be evicted by another burst job"
            )
            _assert_scontrol_state(c, burst_a_id, "RUNNING", "burst-a after guard")
            assert job_state(sq, burst_b_id) == "PD", (
                "pending burst job must wait — it cannot preempt an equal-priority burst job"
            )
            _assert_scontrol_state(c, burst_b_id, "PENDING", "burst-b after guard")
        finally:
            if burst_a_id is not None:
                c.cli_allow_fail(["scancel", str(burst_a_id)])
            if burst_b_id is not None:
                c.cli_allow_fail(["scancel", str(burst_b_id)])


class TestBurstQosNotPreemptedByQosWithoutAllowList:
    """A QoS that does NOT list burst in its preempt allow-list must not evict a
    burst job, even when its priority gap would otherwise qualify it under a plain
    priority-based preemption scheme."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_burst_qos_not_preempted_by_qos_without_allow_list_entry(
        self, accounting_cluster
    ):
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=burst-target",  "priority=-5000"])
        # stranger has a huge priority advantage but no preempt= entry at all.
        c.sacctmgr(["add", "qos", "name=stranger", "priority=100000"])
        time.sleep(15)

        burst_id = None
        stranger_id = None
        try:
            burst_script = c.write_file("burst-target.sh", _SLEEP_SCRIPT)
            burst_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "burst-target", burst_script])
            )
            assert burst_id is not None, "burst submit failed"
            wait_job_state(c, burst_id, "R", timeout=30)
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst initial")

            stranger_script = c.write_file("stranger.sh", _SLEEP_SCRIPT)
            stranger_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "stranger", stranger_script])
            )
            assert stranger_id is not None, "stranger submit failed"
            wait_job_state(c, stranger_id, "PD", timeout=30)
            _assert_scontrol_state(c, stranger_id, "PENDING", "stranger before guard")

            # Under qos_priority mode the allow-list is the gate — priority gap
            # alone is not sufficient. burst must stay running.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, burst_id) == "R", (
                "burst job must not be evicted by a QoS that lacks it in its preempt allow-list"
            )
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst after guard")
            assert job_state(sq, stranger_id) == "PD", (
                "stranger must stay pending — it has no preempt allow-list entry for burst"
            )
            _assert_scontrol_state(c, stranger_id, "PENDING", "stranger after guard")
        finally:
            if burst_id is not None:
                c.cli_allow_fail(["scancel", str(burst_id)])
            if stranger_id is not None:
                c.cli_allow_fail(["scancel", str(stranger_id)])


class TestBurstQosFullWorkflow:
    """End-to-end overflow scenario:
      1. Default QoS hits its per-user node cap (maxtresperuser=node=1).
      2. User submits a second job under burst QoS — it runs on the overflow slot.
      3. A normal job arrives — burst job is preempted, normal job takes the slot.

    This exercises the complete burst-QoS lifecycle from overflow admission
    through eviction in a single test."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_burst_qos_full_workflow_capacity_overflow_then_preemption(
        self, accounting_cluster
    ):
        c = accounting_cluster

        # burst-flow must be created first so normal-cap can reference it in
        # its preempt allow-list (sacctmgr validates the allow-list at creation).
        c.sacctmgr(["add", "qos", "name=burst-flow", "priority=-5000"])
        c.sacctmgr(["add", "qos", "name=normal-cap",
                    "priority=0", "maxtresperuser=node=1",
                    "preempt=burst-flow"])
        time.sleep(15)

        cap_id = None
        burst_id = None
        normal_id = None
        try:
            node0 = c.node_names[0]

            # [1] normal-cap job fills the single allowed node.
            cap_script = c.write_file("cap-job.sh", _SLEEP_SCRIPT)
            cap_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node0}",
                          "-q", "normal-cap", cap_script])
            )
            assert cap_id is not None, "cap job submit failed"
            wait_job_state(c, cap_id, "R", timeout=30)
            _assert_scontrol_state(c, cap_id, "RUNNING", "cap job initial")

            # [2] A second normal-cap job is blocked by the per-user cap.
            blocked_script = c.write_file("blocked-job.sh", _SLEEP_SCRIPT)
            blocked_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node0}",
                          "-q", "normal-cap", blocked_script])
            )
            assert blocked_id is not None, "blocked job submit failed"
            time.sleep(6)
            sq = c.squeue_all()
            assert job_state(sq, blocked_id) == "PD", (
                "second normal-cap job must be blocked by the per-user node cap"
            )
            _assert_scontrol_state(c, blocked_id, "PENDING", "blocked by cap")
            c.cli_allow_fail(["scancel", str(blocked_id)])

            # [3] User submits under burst-flow QoS — no per-user node cap applies.
            # Pin it to node0 so it contends for the exact slot the normal
            # preemptor targets in [5]. Without this pin, a multi-node cluster
            # places burst on a different free node, the freed node0 satisfies the
            # preemptor directly, and no preemption ever fires. node0 is still held
            # exclusively by the cap job, so burst pends until [4] frees it.
            burst_script = c.write_file("burst-flow-job.sh", _SLEEP_SCRIPT)
            burst_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node0}",
                          "-q", "burst-flow", burst_script])
            )
            assert burst_id is not None, "burst-flow job submit failed"
            wait_job_state(c, burst_id, "PD", timeout=30)
            _assert_scontrol_state(c, burst_id, "PENDING", "burst-flow waiting for node0")

            # [4] Cancel the cap job to free node0 so burst can overflow onto it.
            c.cli_allow_fail(["scancel", str(cap_id)])
            cap_id = None

            # Now burst-flow runs on the freed node0.
            wait_job_state(c, burst_id, "R", timeout=30)
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst-flow running")

            # [5] Normal job arrives — must preempt the burst job.
            normal_script = c.write_file("normal-preemptor.sh", _QUICK_SCRIPT)
            normal_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node0}",
                          "-q", "normal-cap", normal_script])
            )
            assert normal_id is not None, "normal preemptor submit failed"
            # A fast scheduler may fire preemption before this poll sees PD — the
            # normal job then goes directly to R. We verify admission (not
            # rejected) and let the rest of the test confirm the eviction outcome.
            time.sleep(4)
            sq = c.squeue_all()
            normal_state = job_state(sq, normal_id)
            assert normal_state in ("PD", "R"), (
                f"normal preemptor must be admitted (PD or R), not rejected; got {normal_state!r}"
            )

            terminal = wait_job(c, burst_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"burst-flow job must be preempted by normal job; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, burst_id, "CANCELLED", "burst-flow after preemption")

            wait_job_state(c, normal_id, "R", timeout=30)
            _assert_scontrol_state(c, normal_id, "RUNNING", "normal after preemption")

            final = wait_job(c, normal_id, timeout=30)
            assert final == "CD", f"normal preemptor must complete; got {final!r}"
        finally:
            for jid in (cap_id, burst_id, normal_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])


class TestBurstQosRequeueMode:
    """When a burst job's QoS sets preempt_mode=requeue, eviction must requeue
    the burst job to PENDING (not cancel it), and it must restart automatically
    once the normal job releases the node."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _BASE_CONFIG

    def test_burst_qos_requeue_mode_restores_job_to_pending(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        # burst-rq: low priority, preemptmode=requeue so it survives eviction.
        c.sacctmgr(["add", "qos", "name=burst-rq",
                    "priority=-5000", "preemptmode=requeue"])
        c.sacctmgr(["add", "qos", "name=normal-rq",
                    "priority=0", "preempt=burst-rq"])
        time.sleep(15)

        burst_id = None
        normal_id = None
        try:
            burst_script = c.write_file("burst-rq-victim.sh", _SLEEP_SCRIPT)
            burst_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "burst-rq", burst_script])
            )
            assert burst_id is not None, "burst-rq submit failed"
            wait_job_state(c, burst_id, "R", timeout=30)
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst-rq initial")

            normal_script = c.write_file("normal-rq-aggressor.sh", _QUICK_SCRIPT)
            normal_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}", "-q", "normal-rq", normal_script])
            )
            assert normal_id is not None, "normal-rq submit failed"
            wait_job_state(c, normal_id, "PD", timeout=30)
            _assert_scontrol_state(c, normal_id, "PENDING", "normal-rq before preemption")

            # Burst must be requeued (PD), not cancelled.
            wait_job_state(c, burst_id, "PD", timeout=_WAIT_PREEMPT)
            _assert_scontrol_state(c, burst_id, "PENDING", "burst-rq after requeue")

            # Normal takes the slot and runs.
            wait_job_state(c, normal_id, "R", timeout=30)
            _assert_scontrol_state(c, normal_id, "RUNNING", "normal-rq running")

            # Burst must stay pending while normal holds the node.
            assert job_state(c.squeue_all(), burst_id) == "PD", (
                "requeued burst job must remain pending while normal holds the node"
            )
            _assert_scontrol_state(c, burst_id, "PENDING", "burst-rq while normal runs")

            final = wait_job(c, normal_id, timeout=30)
            assert final == "CD", f"normal-rq must complete; got {final!r}"

            # Once the node is free, burst must restart automatically.
            wait_job_state(c, burst_id, "R", timeout=_WAIT_RESUME)
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst-rq after resuming")
        finally:
            if burst_id is not None:
                c.cli_allow_fail(["scancel", str(burst_id)])
            if normal_id is not None:
                c.cli_allow_fail(["scancel", str(normal_id)])


class TestBurstQosPartitionOffBlocksPreemption:
    """Even when the normal QoS has burst in its allow-list and the priority gap
    qualifies, a partition with preempt_mode=off must block eviction entirely."""

    @pytest.fixture
    def cluster_config_overrides(self):
        # Override the base config: partition preempt_mode=off.
        return {
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "preempt_mode": "off",
                }
            ],
            "scheduler": {
                "preempt_type": "qos_priority",
            },
            "auth": {"plugin": "none", "allow_root_jobs": True},
        }

    def test_burst_qos_partition_off_blocks_preemption(self, accounting_cluster):
        c = accounting_cluster
        node = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=burst-noevict",  "priority=-5000"])
        c.sacctmgr(["add", "qos", "name=normal-noevict",
                    "priority=0", "preempt=burst-noevict"])
        time.sleep(15)

        burst_id = None
        normal_id = None
        try:
            burst_script = c.write_file("burst-noevict.sh", _SLEEP_SCRIPT)
            burst_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}",
                          "-q", "burst-noevict", burst_script])
            )
            assert burst_id is not None, "burst-noevict submit failed"
            wait_job_state(c, burst_id, "R", timeout=30)
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst initial")

            normal_script = c.write_file("normal-noevict.sh", _QUICK_SCRIPT)
            normal_id = parse_job_id(
                c.sbatch(["-N1", "--exclusive", f"--nodelist={node}",
                          "-q", "normal-noevict", normal_script])
            )
            assert normal_id is not None, "normal-noevict submit failed"
            wait_job_state(c, normal_id, "PD", timeout=30)
            _assert_scontrol_state(c, normal_id, "PENDING", "normal before guard")

            # Allow-list and priority gap both say "preempt", but partition says "off".
            # Partition gate wins — burst must stay running.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, burst_id) == "R", (
                "burst job must not be evicted when partition preempt_mode=off, "
                "even when QoS allow-list and priority gap both qualify preemption"
            )
            _assert_scontrol_state(c, burst_id, "RUNNING", "burst after guard")
            assert job_state(sq, normal_id) == "PD", (
                "normal job must stay pending — partition gate blocks preemption"
            )
            _assert_scontrol_state(c, normal_id, "PENDING", "normal after guard")
        finally:
            if burst_id is not None:
                c.cli_allow_fail(["scancel", str(burst_id)])
            if normal_id is not None:
                c.cli_allow_fail(["scancel", str(normal_id)])
