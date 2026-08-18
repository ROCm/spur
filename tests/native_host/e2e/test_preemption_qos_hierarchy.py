# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for the QOS preemption hierarchy and preempt_exempt_time features.

Covers four scenarios:

1. preempt_type=qos_priority blocks preemption when the pending job's QOS
   allow-list does not include the running job's QOS.

2. preempt_type=qos_priority allows preemption when the pending job's QOS
   allow-list explicitly includes the running job's QOS.

3. preempt_exempt_time protects a recently-started job from preemption for
   a configurable window (wall-clock test with a short window).

4. scontrol reconfigure preserves a per-partition preempt_exempt_time that
   was set via scontrol update-partition, confirming the fix for the
   reconfigure-wipe bug.

Tests 1 and 2 require Postgres (accounting_cluster fixture, skips when Docker
is unavailable). Tests 3 and 4 only need the base cluster fixture.
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

# Merged into every cluster_config_overrides fixture so that jobs submitted
# by the root user (the typical CI / developer case) are not rejected by spurd.
_AUTH_ALLOW_ROOT = {"auth": {"plugin": "none", "allow_root_jobs": True}}


class TestQosPreemptHierarchyBlocked:
    """With preempt_type=qos_priority, a high-priority job whose QOS has an
    empty preempt allow-list must NOT preempt a running job, even with a
    priority gap well above 2×."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
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
            **_AUTH_ALLOW_ROOT,
        }

    def test_preemption_blocked_when_qos_not_in_allow_list(self, accounting_cluster):
        c = accounting_cluster
        node0 = c.node_names[0]

        # "low" QOS: lower priority. "high" QOS: much higher priority but
        # with an empty preempt allow-list — it is not allowed to preempt
        # any other QOS under qos_priority mode.
        c.sacctmgr(["add", "qos", "name=low-hier", "priority=100", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=high-hier", "priority=100000"])
        # No "preempt=..." set on high-hier => allow-list is empty.
        time.sleep(15)  # wait past QoS cache refresh floor

        low_id = None
        try:
            low_script = c.write_file(
                "hier-blocked-low.sh", "#!/bin/bash\nsleep 600\n"
            )
            low_out = c.sbatch(
                ["-J", "hier-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "low-hier", low_script]
            )
            low_id = parse_job_id(low_out)
            assert low_id is not None, f"submit failed:\n{low_out}"
            wait_job_state(c, low_id, "R", timeout=30)

            high_script = c.write_file(
                "hier-blocked-high.sh", "#!/bin/bash\nsleep 2\n"
            )
            c.sbatch(
                ["-J", "hier-high", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "high-hier", high_script]
            )

            # Give the scheduler several cycles to attempt preemption.
            time.sleep(10)

            # low job must still be running — high-hier has an empty
            # preempt allow-list so it cannot preempt low-hier.
            state = c.scontrol("show", "job", str(low_id))
            assert "JobState=RUNNING" in state, (
                "low job must remain RUNNING when pending QOS allow-list is empty"
            )
        finally:
            if low_id is not None:
                c.cli_allow_fail(["scancel", str(low_id)])
            c.cli_allow_fail(["scancel", "--name=hier-high"])


class TestQosPreemptHierarchyAllowed:
    """With preempt_type=qos_priority, a high-priority job whose QOS explicitly
    lists the running job's QOS in its preempt allow-list MUST preempt."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
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
            **_AUTH_ALLOW_ROOT,
        }

    def test_preemption_fires_when_qos_in_allow_list(self, accounting_cluster):
        c = accounting_cluster
        node0 = c.node_names[0]

        c.sacctmgr(["add", "qos", "name=low-allow", "priority=100", "preemptmode=cancel"])
        # "high-allow" explicitly lists "low-allow" in its preempt allow-list.
        c.sacctmgr(
            ["add", "qos", "name=high-allow", "priority=100000",
             "preempt=low-allow"]
        )
        time.sleep(15)

        low_id = None
        try:
            low_script = c.write_file(
                "hier-allow-low.sh", "#!/bin/bash\nsleep 600\n"
            )
            low_out = c.sbatch(
                ["-J", "allow-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "low-allow", low_script]
            )
            low_id = parse_job_id(low_out)
            assert low_id is not None, f"submit failed:\n{low_out}"
            wait_job_state(c, low_id, "R", timeout=30)

            high_script = c.write_file(
                "hier-allow-high.sh", "#!/bin/bash\nsleep 2\n"
            )
            high_out = c.sbatch(
                ["-J", "allow-high", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", "-q", "high-allow", high_script]
            )
            high_id = parse_job_id(high_out)
            assert high_id is not None, f"submit failed:\n{high_out}"

            # low must be preempted (cancelled) and high must complete.
            wait_job_state(c, low_id, "CA", timeout=30)
            high_state = wait_job(c, high_id, timeout=30)
            assert high_state == "CD", (
                f"high-allow job did not complete: {high_state}"
            )
        finally:
            if low_id is not None:
                c.cli_allow_fail(["scancel", str(low_id)])


class TestPreemptExemptTime:
    """preempt_exempt_time protects a recently-started job for the configured
    window, then stops protecting it once the window has elapsed."""

    # Use a short exempt window so the test doesn't take forever.
    EXEMPT_SECS = 20
    SAFE_WAIT_SECS = 8   # comfortably inside the window
    AFTER_WINDOW_WAIT = EXEMPT_SECS + 10  # comfortably past the window

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
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
                "preempt_exempt_time": self.EXEMPT_SECS,
            },
            **_AUTH_ALLOW_ROOT,
        }

    def test_exempt_window_protects_then_expires(self, cluster):
        c = cluster
        node0 = c.node_names[0]

        low_id = None
        try:
            low_script = c.write_file(
                "exempt-low.sh", "#!/bin/bash\nsleep 600\n"
            )
            low_out = c.sbatch(
                ["-J", "exempt-low", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", low_script]
            )
            low_id = parse_job_id(low_out)
            assert low_id is not None, f"submit failed:\n{low_out}"
            wait_job_state(c, low_id, "R", timeout=60)

            # Submit the high-priority job immediately after low starts.
            high_script = c.write_file(
                "exempt-high.sh", "#!/bin/bash\nsleep 2\n"
            )
            high_out = c.sbatch(
                ["-J", "exempt-high", "-N", "1", f"--nodelist={node0}",
                 "--exclusive", high_script]
            )
            high_id = parse_job_id(high_out)
            assert high_id is not None, f"submit failed:\n{high_out}"
            # Force high's priority above the 2× threshold.
            c.scontrol("update", f"JobId={high_id}", "Priority=1000000")

            # Within the exempt window: low must still be running.
            time.sleep(self.SAFE_WAIT_SECS)
            state = c.scontrol("show", "job", str(low_id))
            assert "JobState=RUNNING" in state, (
                f"low job must be protected within the {self.EXEMPT_SECS}s "
                f"exempt window (checked after {self.SAFE_WAIT_SECS}s)"
            )

            # After the window expires, preemption must fire.
            time.sleep(self.AFTER_WINDOW_WAIT)
            wait_job_state(c, low_id, "CA", timeout=30)
            high_state = wait_job(c, high_id, timeout=30)
            assert high_state == "CD", (
                f"high-priority job did not complete after exempt window: {high_state}"
            )
        finally:
            if low_id is not None:
                c.cli_allow_fail(["scancel", str(low_id)])


class TestReconfigurePreservesExemptTime:
    """scontrol reconfigure must not wipe a per-partition preempt_exempt_time
    override that was set live via scontrol update-partition."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "preempt_mode": "cancel",
                    # No preempt_exempt_time in TOML — starts at None (inherit global=0).
                }
            ],
            **_AUTH_ALLOW_ROOT,
        }

    def test_reconfigure_preserves_partition_exempt_time(self, cluster):
        c = cluster

        # Set a per-partition exempt time via scontrol inline syntax.
        c.scontrol("update", "PartitionName=default", "PreemptExemptTime=120")

        # Confirm it's visible in scontrol show partition.
        out = c.scontrol("show", "partition", "default")
        assert "PreemptExemptTime=120" in out, (
            f"preempt_exempt_time not set after scontrol update: {out}"
        )

        # Trigger a reconfigure.
        c.scontrol("reconfigure")
        time.sleep(3)  # let the controller apply the reload

        # The override must survive: it was set at runtime, not from TOML.
        # In Spur, a live scontrol update writes a WAL entry; reconfigure
        # sends its own WAL update from the TOML values. After this fix,
        # the reconfigure WAL entry carries preempt_exempt_time=None (no change)
        # for partitions whose TOML does not have the field set, so the
        # runtime value is preserved.
        out_after = c.scontrol("show", "partition", "default")
        assert "PreemptExemptTime=120" in out_after, (
            f"preempt_exempt_time was wiped by reconfigure:\nbefore: {out}\nafter: {out_after}"
        )
