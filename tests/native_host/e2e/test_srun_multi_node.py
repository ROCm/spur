# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for standalone and step-mode srun task fan-out."""

import re
import time

from cluster import job_state, parse_job_id, wait_job, wait_job_state


class TestStandaloneSrun:
    def test_srun_two_nodes_two_tasks(self, multi_node_cluster):
        cluster = multi_node_cluster
        code, out = cluster.srun_with_exit(
            [
                "-N",
                "2",
                "-n",
                "2",
                "bash",
                "-c",
                'echo "host=$(hostname) rank=${SPUR_PROCID}"',
            ]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        lines = [ln for ln in out.splitlines() if ln.startswith("host=")]
        assert len(lines) == 2, f"expected 2 task lines, got {len(lines)}:\n{out}"

        hosts = {ln.split("host=")[1].split()[0] for ln in lines}
        assert len(hosts) == 2, f"expected 2 distinct hosts, got {hosts}:\n{out}"

        ranks = {ln.split("rank=")[1].strip() for ln in lines}
        assert ranks == {"0", "1"}, f"expected ranks 0 and 1, got {ranks}:\n{out}"

    def test_srun_holds_allocation_until_step_finishes(self, multi_node_cluster):
        cluster = multi_node_cluster
        log = f"{cluster.remote_dir}/srun-hold.log"
        cmd = (
            f"SPUR_CONTROLLER_ADDR='{cluster.controller_addr}' "
            f"PATH='{cluster.bin_dir}':$PATH "
            f"nohup '{cluster.bin_dir}/srun' -N 2 sleep 8 > '{log}' 2>&1 & echo $!"
        )
        cluster.nodes[0].exec(cmd)
        time.sleep(2)

        sq = cluster.squeue_all()
        running = [
            int(m.group(1))
            for m in re.finditer(r"(\d+)\s+\S+\s+\S+\s+\S+\s+R\b", sq)
        ]
        assert running, f"expected a running srun allocation in squeue:\n{sq}"
        job_id = running[0]

        wait_job_state(cluster, job_id, "R", timeout=30)
        sinfo = cluster.sinfo()
        assert not cluster._cluster_is_ready(sinfo), (
            f"expected allocated nodes while srun sleep runs, sinfo:\n{sinfo}"
        )

        wait_job(cluster, job_id, timeout=60)
        deadline = time.time() + 60
        while time.time() < deadline:
            if cluster._cluster_is_ready(cluster.sinfo()):
                return
            time.sleep(2)
        raise TimeoutError("nodes did not return to idle after srun completed")


class TestSrunInBatch:
    def test_sbatch_srun_hostname_on_allocated_nodes(self, multi_node_cluster):
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/srun-step.out"
        script = cluster.write_file(
            "srun-in-batch.sh",
            "#!/bin/bash\n"
            "srun -n 2 bash -c 'echo host=$(hostname) rank=${SPUR_PROCID}'\n",
        )
        sb = cluster.sbatch(["-J", "srun-step", "-N", "2", "-n", "2", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        content = cluster.read_output_all_nodes(out_path)
        lines = [ln for ln in content.splitlines() if ln.startswith("host=")]
        assert len(lines) == 2, f"expected 2 step task lines in batch output:\n{content}"
