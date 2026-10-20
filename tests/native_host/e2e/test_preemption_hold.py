# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E test: the node freed by a preemption goes to the job it was freed for.

Preemption eligibility is decided by QOS rank, but the scheduler pass places
pending jobs by effective priority (QOS base x fair-share x age x tier). A
pending job with a lower QOS but a higher effective priority therefore sits
ahead of the preemptor in the pass. Without a hold it takes the node the
preemption just freed, and the preemptor is left pending to evict again.

Here fair-share supplies the higher effective priority (age would too, but
can't be set from the CLI): `older` has a lower QOS than `preemptor` and no
right to preempt, but a 10x fair-share factor against the preemptor's sunk one.

Requires:
  - preempt_type=qos_priority (scheduler config); preemption is off otherwise
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"

_WAIT_START = 60
_GUARD_SECS = 12

# fairshare_refresh_secs is 10 in the harness config and FairshareCache clamps
# its interval to a 10s floor, so a planted usage row needs two cycles plus
# slack before the scheduler sees it. The QOS cache refreshes on the same
# interval.
_FAIRSHARE_REFRESH_SECS = 30

_HEAVY_USAGE = 999_000_000
_LIGHT_USAGE = 1


def _sql_str(value: str) -> str:
    """Escape a value for embedding as a single-quoted SQL string literal."""
    return value.replace("'", "''")


def _seed_usage(cluster, user: str, account: str, cpu_seconds: int) -> None:
    """Plant a usage row for today so fair-share has history to divide by."""
    cluster.psql(
        "INSERT INTO usage "
        "(user_name, account, period_start, period_end, cpu_seconds, gpu_seconds, job_count) "
        f"VALUES ('{_sql_str(user)}', '{_sql_str(account)}', "
        f"date_trunc('day', NOW()), NOW(), {cpu_seconds}, 0, 1) "
        "ON CONFLICT (user_name, account, period_start) "
        "DO UPDATE SET cpu_seconds = EXCLUDED.cpu_seconds"
    )


class TestPreemptionHoldsFreedNode:
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
            "scheduler": {"preempt_type": "qos_priority"},
            # Required when the test runner SSHes in as root: spurd refuses to
            # execute jobs as uid 0 unless this is explicitly enabled.
            "auth": {"plugin": "none", "allow_root_jobs": True},
        }

    def test_freed_node_goes_to_the_preemptor_not_an_older_lower_qos_job(
        self, accounting_cluster
    ):
        c = accounting_cluster
        node = c.node_names[0]
        user = c.nodes[0].user

        c.sacctmgr(["add", "account", "name=hold-heavy", "fairshare=1"])
        c.sacctmgr(["add", "account", "name=hold-light", "fairshare=10"])
        c.sacctmgr(["add", "user", f"name={user}", "account=hold-heavy"])
        c.sacctmgr(["add", "user", f"name={user}", "account=hold-light"])
        _seed_usage(c, user, "hold-heavy", _HEAVY_USAGE)
        _seed_usage(c, user, "hold-light", _LIGHT_USAGE)
        c.sacctmgr(["add", "qos", "name=hold-burst", "priority=100"])
        c.sacctmgr(["add", "qos", "name=hold-batch", "priority=1000"])
        c.sacctmgr(["add", "qos", "name=hold-primus", "priority=10000",
                    "preempt=hold-burst"])
        time.sleep(_FAIRSHARE_REFRESH_SECS)

        def submit(name: str, account: str, qos: str) -> int:
            script = c.write_file(f"{name}.sh", _SLEEP_SCRIPT)
            job_id = parse_job_id(c.sbatch([
                "-J", name, "-N1", "--exclusive", f"--nodelist={node}",
                "-A", account, "-q", qos, script,
            ]))
            assert job_id is not None, f"{name} submit failed"
            return job_id

        victim_id = older_id = preemptor_id = None
        try:
            victim_id = submit("hold-victim", "hold-heavy", "hold-burst")
            wait_job_state(c, victim_id, "R", timeout=_WAIT_START)

            # hold-batch may preempt nothing, so `older` just waits; its
            # fair-share puts it ahead of the preemptor in every pass.
            older_id = submit("hold-older", "hold-light", "hold-batch")
            wait_job_state(c, older_id, "PD", timeout=30)

            preemptor_id = submit("hold-preemptor", "hold-heavy", "hold-primus")
            wait_job_state(c, preemptor_id, "R", timeout=_WAIT_START)

            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, preemptor_id) == "R", (
                "the preemptor must keep the node its preemption freed"
            )
            assert job_state(sq, older_id) == "PD", (
                "a job ahead of the preemptor in the pass must not take the node "
                "freed for the preemptor"
            )
        finally:
            for jid in (victim_id, older_id, preemptor_id):
                if jid is not None:
                    c.cli_allow_fail(["scancel", str(jid)])
