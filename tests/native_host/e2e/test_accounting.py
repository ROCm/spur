# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for accounting: sacct exit reporting and QoS pending reasons.

Requires Postgres on node 0 (the accounting_cluster fixture, which skips
when Docker is unavailable).
"""

import re
import time

from cluster import parse_job_id, wait_job, wait_sacct_row


class TestSacctExitReporting:
    def test_signal_half_and_derived_exit_code(self, accounting_cluster):
        c = accounting_cluster

        # (1) A job killed by a signal: sacct must show the signal half (0:9),
        # not 0:0. SIGKILL the batch shell itself.
        sig = c.write_file("acct-signal.sh", "#!/bin/bash\nkill -9 $$\n")
        sig_id = parse_job_id(c.sbatch(["-J", "acct-sig", "-N", "1", sig]))
        assert sig_id is not None
        wait_job(c, sig_id, timeout=60)
        row = wait_sacct_row(c, sig_id, "JobID,ExitCode")
        # ExitCode renders code:signal; the signal half is the parity fix.
        assert row.split()[1].endswith(":9"), f"expected signal half :9, got {row!r}"

        # (2) A multi-step job (steps exit 0, 7, 3): Slurm reports
        # ExitCode=last (3:0) and DerivedExitCode=max (7:0).
        multi = c.write_file(
            "acct-multi.sh",
            "#!/bin/bash\n"
            "srun bash -c 'exit 0'\n"
            "srun bash -c 'exit 7'\n"
            "srun bash -c 'exit 3'\n",
        )
        m_id = parse_job_id(c.sbatch(["-J", "acct-multi", "-N", "1", multi]))
        assert m_id is not None
        wait_job(c, m_id, timeout=90)
        row = wait_sacct_row(c, m_id, "JobID,ExitCode,DerivedExitCode")
        fields = row.split()
        assert fields[1] == "3:0", f"expected ExitCode 3:0, got {fields!r}"
        assert fields[2] == "7:0", f"expected DerivedExitCode 7:0, got {fields!r}"


def _reason(cluster, job_id: int) -> str:
    out = cluster.scontrol("show", "job", str(job_id))
    m = re.search(r"Reason=(\S+)", out)
    return m.group(1) if m else ""


class TestQosLimitReasons:
    def test_wall_cap_sets_qos_pending_reason(self, accounting_cluster):
        c = accounting_cluster

        # Define a QoS that caps wall time at 1 minute.
        c.sacctmgr(["add", "qos", "name=short", "maxwall=1"])
        # Wait past the QoS cache refresh floor (10s) before submitting, else the job starts before the cap loads.
        time.sleep(15)

        # A job in that QoS asking for 1h exceeds the cap, so it stays PENDING
        # with the specific QoS reason (not generic Resources/PartitionTimeLimit).
        script = c.write_file("qos-job.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            c.sbatch(["-J", "qos-wall", "-N", "1", "-q", "short", "-t", "60", script])
        )
        assert job_id is not None

        deadline = time.time() + 30
        reason = ""
        while time.time() < deadline:
            reason = _reason(c, job_id)
            if reason == "QOSMaxWallDurationPerJobLimit":
                break
            time.sleep(2)
        assert reason == "QOSMaxWallDurationPerJobLimit", (
            f"expected QOSMaxWallDurationPerJobLimit, got {reason!r}"
        )
