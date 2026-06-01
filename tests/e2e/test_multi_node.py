# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Multi-node E2E tests for the Spur scheduler.

These tests require at least 2 nodes in SPUR_TEST_NODES.
The multi_node_cluster fixture validates this and skips if insufficient.
"""

import time

from cluster import parse_job_id, job_state, wait_job


class TestMultiNodeDispatch:
    def test_two_node_job_completes(self, multi_node_cluster):
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/two-node.out"
        script = cluster.write_script(
            "two-node.sh",
            "#!/bin/bash\n"
            'echo "node=$(hostname)"\n'
            'echo "SPUR_JOB_ID=${SPUR_JOB_ID}"\n'
            'echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"\n'
            'echo "SPUR_NUM_NODES=${SPUR_NUM_NODES}"\n'
            'echo "SPUR_PEER_NODES=${SPUR_PEER_NODES}"\n'
            "echo TWO_NODE_OK\n",
        )
        sb = cluster.sbatch(["-J", "test-2node", "-N", "2", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        assert "TWO_NODE_OK" in all_output, f"missing TWO_NODE_OK:\n{all_output}"
        assert "SPUR_NUM_NODES=2" in all_output, f"missing SPUR_NUM_NODES=2:\n{all_output}"
        assert "SPUR_NODE_RANK=" in all_output, f"missing SPUR_NODE_RANK:\n{all_output}"
        assert any(
            l.startswith("SPUR_PEER_NODES=") and len(l) > len("SPUR_PEER_NODES=")
            for l in all_output.splitlines()
        ), f"SPUR_PEER_NODES should be non-empty:\n{all_output}"

    def test_distributed_env_vars(self, multi_node_cluster):
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/dist-env.out"
        script = cluster.write_script(
            "dist-env.sh",
            "#!/bin/bash\n"
            'echo "RANK=${RANK}"\n'
            'echo "WORLD_SIZE=${WORLD_SIZE}"\n'
            'echo "MASTER_ADDR=${MASTER_ADDR}"\n'
            'echo "MASTER_PORT=${MASTER_PORT}"\n'
            "echo DIST_ENV_OK\n",
        )
        sb = cluster.sbatch(["-J", "dist-env", "-N", "2", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        assert "WORLD_SIZE=2" in all_output
        assert "RANK=0" in all_output
        assert "RANK=1" in all_output
        assert "MASTER_PORT=29500" in all_output
        master_addr_lines = [
            l for l in all_output.splitlines()
            if l.startswith("MASTER_ADDR=") and l != "MASTER_ADDR="
        ]
        assert len(master_addr_lines) >= 2, (
            f"MASTER_ADDR should be set on both ranks:\n{all_output}"
        )


class TestMultiNodeScheduling:
    def test_nodelist_runs_on_requested_node(self, multi_node_cluster):
        cluster = multi_node_cluster
        target = cluster.node_names[0]
        out_path = f"{cluster.remote_dir}/nodelist-{target}.out"
        script = cluster.write_script(
            "nodename.sh",
            '#!/bin/bash\necho "RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}"\n',
        )
        sb = cluster.sbatch(["-J", "nodelist", "-N", "1", "-w", target, "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=60)
        content = cluster.read_output_on_any_node(out_path)
        assert f"RAN_ON={target}" in content, f"expected run on {target}, got:\n{content}"

    def test_nodelist_runs_on_second_node(self, multi_node_cluster):
        cluster = multi_node_cluster
        target = cluster.node_names[1]
        out_path = f"{cluster.remote_dir}/nodelist-{target}.out"
        script = cluster.write_script(
            "nodename2.sh",
            '#!/bin/bash\necho "RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}"\n',
        )
        sb = cluster.sbatch(["-J", "nodelist2", "-N", "1", "-w", target, "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=60)
        content = cluster.read_output_on_any_node(out_path)
        assert f"RAN_ON={target}" in content, f"expected run on {target}, got:\n{content}"

    def test_exclude_skips_node(self, multi_node_cluster):
        cluster = multi_node_cluster
        excluded = cluster.node_names[0]
        out_path = f"{cluster.remote_dir}/exclude.out"
        script = cluster.write_script(
            "nodename-ex.sh",
            '#!/bin/bash\necho "RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}"\n',
        )
        sb = cluster.sbatch(["-J", "exclude", "-N", "1", "-x", excluded, "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=60)
        content = cluster.read_output_on_any_node(out_path)
        assert f"RAN_ON={excluded}" not in content, (
            f"job must not run on excluded node {excluded}, got:\n{content}"
        )
        allowed = cluster.node_names[1:]
        assert any(f"RAN_ON={n}" in content for n in allowed), (
            f"expected run on one of {allowed}, got:\n{content}"
        )

    def test_concurrent_jobs_on_two_nodes(self, multi_node_cluster):
        cluster = multi_node_cluster
        out1 = f"{cluster.remote_dir}/con1.out"
        out2 = f"{cluster.remote_dir}/con2.out"
        script = cluster.write_script(
            "concurrent.sh",
            "#!/bin/bash\necho CONCURRENT_START\nsleep 5\necho CONCURRENT_DONE\n",
        )

        sb1 = cluster.sbatch(["-J", "con1", "-N", "1", "-o", out1, script])
        sb2 = cluster.sbatch(["-J", "con2", "-N", "1", "-o", out2, script])
        j1 = parse_job_id(sb1)
        j2 = parse_job_id(sb2)
        assert j1 is not None and j2 is not None

        time.sleep(3)
        sq = cluster.squeue_all()
        assert job_state(sq, j1) == "R"
        assert job_state(sq, j2) == "R"

        wait_job(cluster, j1, timeout=60)
        wait_job(cluster, j2, timeout=60)

        c1 = cluster.read_output_on_any_node(out1)
        c2 = cluster.read_output_on_any_node(out2)
        assert "CONCURRENT_DONE" in c1, f"job1 missing CONCURRENT_DONE:\n{c1}"
        assert "CONCURRENT_DONE" in c2, f"job2 missing CONCURRENT_DONE:\n{c2}"
