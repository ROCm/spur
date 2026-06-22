# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for sacct accounting: exit:signal and DerivedExitCode (issue #297).

Requires spurdbd + Postgres on node 0 (the accounting_cluster fixture, which
skips when Docker or the accounting binaries are unavailable).
"""

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
        row = wait_sacct_row(c, sig_id, "%i %x")
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
        row = wait_sacct_row(c, m_id, "%i %x %X")
        fields = row.split()
        assert fields[1] == "3:0", f"expected ExitCode 3:0, got {fields!r}"
        assert fields[2] == "7:0", f"expected DerivedExitCode 7:0, got {fields!r}"
