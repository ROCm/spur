# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Multi-node E2E tests for the Spur scheduler.

These tests require at least 2 nodes in SPUR_TEST_NODES.
The multi_node_cluster fixture validates this and skips if insufficient.
"""

import time

from cluster import parse_job_id, job_state, wait_job, wait_job_state


def _wait_node_state(cluster, node_name, target_states, timeout=60):
    """Poll sinfo until a node reaches one of the target states."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            state = cluster.sinfo_nodes().get(node_name)
            if state is not None:
                for target in target_states:
                    if state.startswith(target):
                        return state
        except Exception:
            pass
        time.sleep(2)
    raise TimeoutError(
        f"Node {node_name} did not reach {target_states} within {timeout}s"
    )


class TestMultiNodeDispatch:
    def test_two_node_job_completes(self, multi_node_cluster):
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/two-node.out"
        script = cluster.write_file(
            "two-node.sh",
            "#!/bin/bash\n"
            'echo "node=$(hostname)"\n'
            'echo "SPUR_JOB_ID=${SPUR_JOB_ID}"\n'
            'echo "SLURM_JOB_ID=${SLURM_JOB_ID}"\n'
            'echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"\n'
            'echo "SPUR_NNODES=${SPUR_NNODES}"\n'
            'echo "SLURM_NNODES=${SLURM_NNODES}"\n'
            'echo "SPUR_PEER_NODES=${SPUR_PEER_NODES}"\n'
            "echo TWO_NODE_OK\n",
        )
        sb = cluster.sbatch(["-J", "test-2node", "-N", "2", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        assert "TWO_NODE_OK" in all_output, f"missing TWO_NODE_OK:\n{all_output}"
        assert "SPUR_NNODES=2" in all_output, f"missing SPUR_NNODES=2:\n{all_output}"
        assert "SLURM_NNODES=2" in all_output, f"missing SLURM_NNODES=2:\n{all_output}"
        assert "SPUR_NODE_RANK=" in all_output, f"missing SPUR_NODE_RANK:\n{all_output}"
        assert any(
            line.startswith("SPUR_PEER_NODES=") and len(line) > len("SPUR_PEER_NODES=")
            for line in all_output.splitlines()
        ), f"SPUR_PEER_NODES should be non-empty:\n{all_output}"
        # SLURM twins for prefixed vars
        assert "SLURM_JOB_ID=" in all_output, f"missing SLURM_JOB_ID:\n{all_output}"

    def test_distributed_env_vars(self, multi_node_cluster):
        # Spur does not inject the PyTorch rendezvous variables (MASTER_ADDR,
        # MASTER_PORT, WORLD_SIZE, RANK) — Slurm never sets them either. It
        # exposes the allocation topology through SPUR_*/SLURM_* instead.
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/dist-env.out"
        script = cluster.write_file(
            "dist-env.sh",
            "#!/bin/bash\n"
            'echo "SPUR_NNODES=${SPUR_NNODES}"\n'
            'echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"\n'
            'echo "SPUR_PEER_NODES=${SPUR_PEER_NODES}"\n'
            'echo "TORCH_RANK=[${RANK}]"\n'
            'echo "TORCH_WORLD_SIZE=[${WORLD_SIZE}]"\n'
            'echo "TORCH_MASTER_PORT=[${MASTER_PORT}]"\n'
            "echo DIST_ENV_OK\n",
        )
        sb = cluster.sbatch(["-J", "dist-env", "-N", "2", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        assert "SPUR_NNODES=2" in all_output, f"missing SPUR_NNODES=2:\n{all_output}"
        assert "SPUR_NODE_RANK=0" in all_output, f"missing rank 0:\n{all_output}"
        assert "SPUR_NODE_RANK=1" in all_output, f"missing rank 1:\n{all_output}"
        assert any(
            line.startswith("SPUR_PEER_NODES=") and len(line) > len("SPUR_PEER_NODES=")
            for line in all_output.splitlines()
        ), f"SPUR_PEER_NODES should be non-empty:\n{all_output}"
        # The torch names must be empty because Spur no longer injects them.
        assert "TORCH_RANK=[]" in all_output, f"RANK should be unset:\n{all_output}"
        assert "TORCH_WORLD_SIZE=[]" in all_output, (
            f"WORLD_SIZE should be unset:\n{all_output}"
        )
        assert "TORCH_MASTER_PORT=[]" in all_output, (
            f"MASTER_PORT should be unset:\n{all_output}"
        )

    def test_user_rendezvous_env_preserved(self, multi_node_cluster):
        # Regression for #783: a user-exported MASTER_PORT/WORLD_SIZE must
        # survive on a multi-node job, not be overwritten by Spur.
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/user-rdzv.out"
        script = cluster.write_file(
            "user-rdzv.sh",
            "#!/bin/bash\n"
            'echo "MASTER_PORT=${MASTER_PORT}"\n'
            'echo "WORLD_SIZE=${WORLD_SIZE}"\n'
            "echo USER_RDZV_OK\n",
        )
        cmd = (
            f"SPUR_CONTROLLER_ADDR='{cluster.controller_addr}' "
            f"PATH='{cluster.bin_dir}':$PATH "
            f"MASTER_PORT=29999 WORLD_SIZE=16 "
            f"'{cluster.bin_dir}/sbatch' -J user-rdzv -N 2 "
            f"-o '{out_path}' --export=ALL '{script}'"
        )
        sb = cluster.nodes[0].exec(cmd)
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        assert "USER_RDZV_OK" in all_output, f"missing USER_RDZV_OK:\n{all_output}"
        assert "MASTER_PORT=29999" in all_output, (
            f"user MASTER_PORT was overwritten:\n{all_output}"
        )
        assert "WORLD_SIZE=16" in all_output, (
            f"user WORLD_SIZE was overwritten:\n{all_output}"
        )


class TestMultiNodeScheduling:
    def test_nodelist_runs_on_requested_node(self, multi_node_cluster):
        cluster = multi_node_cluster
        target = cluster.node_names[0]
        out_path = f"{cluster.remote_dir}/nodelist-{target}.out"
        script = cluster.write_file(
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
        script = cluster.write_file(
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
        script = cluster.write_file(
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
        script = cluster.write_file(
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


class TestBackfillReservation:
    """A pending 2-node job must reserve its second node instead of letting
    a smaller job dispatch onto it once the node frees up."""

    def test_large_multinode_job_is_not_starved_by_a_smaller_job(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        n0, n1 = cluster.node_names[0], cluster.node_names[1]

        filler_script = cluster.write_file(
            "backfill-filler.sh", "#!/bin/bash\nsleep 8\n"
        )
        filler_id = parse_job_id(
            cluster.sbatch(
                ["-J", "filler", "-N", "1", f"--nodelist={n0}",
                 "--exclusive", "--time=00:01:00", filler_script]
            )
        )
        assert filler_id is not None
        wait_job_state(cluster, filler_id, "R", timeout=30)

        # Pinned to n0/n1 specifically: a >2-node cluster (e.g. CI's default 4)
        # would otherwise let this dispatch immediately onto other free nodes.
        big_out = f"{cluster.remote_dir}/backfill-big.out"
        big_script = cluster.write_file(
            "backfill-big.sh", "#!/bin/bash\necho BIG_RAN\n"
        )
        big_id = parse_job_id(
            cluster.sbatch(
                ["-J", "backfill-big", "-N", "2", f"--nodelist={n0},{n1}",
                 "-o", big_out, big_script]
            )
        )
        assert big_id is not None
        wait_job_state(cluster, big_id, "PD", timeout=15)

        # small requests a duration long enough to still be occupying n1 when
        # big's reservation (~90s out, from filler's 1-min time_limit + grace)
        # would need it — a short-lived small job wouldn't overlap at all and
        # SHOULD be free to backfill into the gap, so this must outlast it.
        small_script = cluster.write_file(
            "backfill-small.sh", "#!/bin/bash\nsleep 200\necho SMALL_RAN\n"
        )
        small_id = parse_job_id(
            cluster.sbatch(
                ["-J", "backfill-small", "-N", "1", f"--nodelist={n1}",
                 "--exclusive", "--time=00:05:00", small_script]
            )
        )
        assert small_id is not None

        time.sleep(10)
        sq = cluster.squeue_all()
        assert job_state(sq, small_id) == "PD", (
            f"small job should stay pending, reserved capacity was taken:\n{sq}"
        )

        n1_show = cluster.scontrol_show_node(n1)
        assert f"PlannedJobId={big_id}" in n1_show, (
            f"n1 is idle-but-reserved for big_id={big_id}, "
            f"scontrol show node should report it:\n{n1_show}"
        )
        assert "State=IDLE+PLANNED" in n1_show, (
            f"n1's State= should carry the PLANNED overlay flag:\n{n1_show}"
        )

        cluster.scancel(str(small_id))

        wait_job(cluster, big_id, timeout=90)
        big_content = cluster.read_output_on_any_node(big_out)
        assert "BIG_RAN" in big_content, f"big job missing BIG_RAN:\n{big_content}"


class TestMultiNodeStateUpdate:
    def test_scontrol_update_drains_multiple_nodes(self, multi_node_cluster):
        cluster = multi_node_cluster
        n0, n1 = cluster.node_names[0], cluster.node_names[1]
        hostlist = f"{n0},{n1}"

        cluster.scontrol("update", f"NodeName={hostlist}",
                         "State=DRAIN", "Reason=e2e-multi")

        for name in [n0, n1]:
            _wait_node_state(cluster, name, ["drain"])

        cluster.scontrol("update", f"NodeName={hostlist}",
                         "State=RESUME")

        for name in [n0, n1]:
            _wait_node_state(cluster, name, ["idle"])
