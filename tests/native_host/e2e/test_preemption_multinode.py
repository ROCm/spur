# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for preemption of multi-node jobs.

When a job spans multiple compute nodes, preemption must reach every node agent —
if even one agent misses the signal, that node stays occupied and the incoming
job cannot start. These tests verify the outcome from the outside: once the
multi-node victim is evicted, the higher-QOS job must be able to acquire all
nodes and run to completion.

Every state transition is verified through both squeue (the primary user-facing
queue view) and scontrol show job (the detailed record view) so that a divergence
between the two surfaces is caught as a test failure rather than silently masked.

Requires:
  - at least 2 nodes in SPUR_TEST_NODES
  - preempt_type=qos_priority (scheduler config); preemption is off otherwise
  - a QOS pair where the aggressor allow-lists the victim and outranks it
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)

NOTE: REST API cross-verification is not yet covered here — the e2e infrastructure
does not have a REST client helper. That is tracked as a separate gap.
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_PREEMPT = 30
_WAIT_RUN = 60

# QOS cache refreshes on the accounting interval; a freshly added QOS needs a
# cycle before the scheduler acts on it.
_CACHE_WARMUP_SECS = 15


# Required when the test runner SSHes in as root: spurd refuses to execute jobs
# as uid 0 unless this is explicitly enabled.
_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}


def _assert_scontrol_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    """Assert JobState=<expected> appears in scontrol show job output."""
    show = cluster.scontrol("show", "job", str(job_id))
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


class TestMultiNodePreemption:
    """A running job that spans multiple nodes must be fully preempted — every
    node freed — so that a higher-QOS job can acquire them all."""

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
            **_AUTH_ROOT,
        }

    def test_preempted_multinode_job_frees_all_nodes(self, accounting_cluster):
        """When a multi-node victim is cancelled by preemption, every node it
        held must be released so that a second multi-node aggressor can start."""
        c = accounting_cluster
        n_nodes = len(c.node_names)
        if n_nodes < 2:
            pytest.skip(
                f"multi-node preemption requires at least 2 nodes in "
                f"SPUR_TEST_NODES (got {n_nodes})"
            )

        c.sacctmgr(["add", "qos", "name=mn-victim-qos", "priority=100",
                    "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=mn-hunter-qos", "priority=10000",
                    "preempt=mn-victim-qos"])
        time.sleep(_CACHE_WARMUP_SECS)

        victim = c.write_file("mn-victim.sh", _SLEEP_SCRIPT)
        aggressor = c.write_file("mn-aggressor.sh", _QUICK_SCRIPT)

        # Victim claims all available nodes.
        victim_id = parse_job_id(
            c.sbatch([f"-N{n_nodes}", "--exclusive", "-q", "mn-victim-qos", victim])
        )
        wait_job_state(c, victim_id, "R", timeout=60)
        _assert_scontrol_state(c, victim_id, "RUNNING", "victim initial")

        # Verify the victim actually spans every node before triggering preemption.
        show = c.scontrol("show", "job", str(victim_id))
        assert f"NumNodes={n_nodes}" in show, (
            f"victim must span all {n_nodes} nodes; got:\n{show}"
        )

        # Aggressor also wants all nodes — it can only start once every agent
        # receives and processes the preemption signal.
        aggressor_id = parse_job_id(
            c.sbatch([f"-N{n_nodes}", "--exclusive", "-q", "mn-hunter-qos", aggressor])
        )
        assert aggressor_id is not None, "aggressor submit failed"

        try:
            terminal = wait_job(c, victim_id, timeout=_WAIT_PREEMPT)
            assert terminal in ("CA", "GONE"), (
                f"multi-node victim must be cancelled on preemption; got {terminal!r}"
            )
            if terminal != "GONE":
                _assert_scontrol_state(c, victim_id, "CANCELLED", "victim after preemption")

            # Aggressor must acquire all nodes and start running.
            # If any agent missed the preemption signal, the aggressor stays pending here.
            wait_job_state(c, aggressor_id, "R", timeout=_WAIT_RUN)
            _assert_scontrol_state(c, aggressor_id, "RUNNING", "aggressor after preemption")

            # Confirm aggressor is actually running on all nodes, not just one.
            show = c.scontrol("show", "job", str(aggressor_id))
            assert f"NumNodes={n_nodes}" in show, (
                f"aggressor must run on all {n_nodes} freed nodes; got:\n{show}"
            )

            final = wait_job(c, aggressor_id, timeout=_WAIT_RUN)
            assert final == "CD", (
                f"aggressor must complete once all nodes are freed; got {final!r}"
            )
        finally:
            c.cli_allow_fail(["scancel", str(victim_id)])
            c.cli_allow_fail(["scancel", str(aggressor_id)])
