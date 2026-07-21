# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for srun --qos (SPUR-77).

Proves an explicit `srun --qos` reaches the controller and is recorded on the
job. Requires Postgres on node 0 (the accounting_cluster fixture) because an
explicit QOS must exist in the controller's QOS cache to be accepted.
"""

import shlex
import time

from cluster import wait_job_state


class TestSrunQos:
    def test_srun_qos_applied_to_job(self, accounting_cluster):
        c = accounting_cluster
        c.sacctmgr(["add", "qos", "name=srunqos"])
        # Wait past the QoS cache refresh floor (10s) before submitting.
        time.sleep(15)

        log = f"{c.remote_dir}/srun-qos.log"
        srun_cmd = " ".join(
            [
                f"SPUR_CONTROLLER_ADDR={shlex.quote(c.controller_addr)}",
                f"PATH={shlex.quote(c.bin_dir)}:$PATH",
                "nohup",
                shlex.quote(f"{c.bin_dir}/srun"),
                "-J", "srun-qos",
                "-q", "srunqos",
                "sleep", "30",
                ">", shlex.quote(log), "2>&1", "&",
                "echo", "$!",
            ]
        )
        pid_out = c.nodes[0].exec(srun_cmd).strip()
        assert pid_out.isdigit(), f"expected background srun pid, got: {pid_out!r}"

        job_ids = []
        deadline = time.time() + 30
        while time.time() < deadline and not job_ids:
            job_ids = c.running_job_ids_by_name("srun-qos")
            if not job_ids:
                time.sleep(1)
        assert job_ids, (
            "expected running srun-qos job in squeue:\n"
            f"{c.squeue(['-n', 'srun-qos', '-t', 'all'])}"
        )
        job_id = job_ids[0]

        try:
            wait_job_state(c, job_id, "R", timeout=30)
            show = c.scontrol("show", "job", str(job_id))
            assert "QOS=srunqos" in show, f"expected QOS=srunqos in job:\n{show}"
        finally:
            c.cli_allow_fail(["scancel", str(job_id)])
