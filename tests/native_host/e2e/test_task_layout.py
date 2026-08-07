# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for task-count defaults and node/CPU allocation layout.

Covers the interaction between -N, -n and --ntasks-per-node: how the task
count is defaulted at submit, and how the controller caps the node count when
a job asks for more nodes than it has tasks to put on them.
"""

import re

from cluster import parse_job_id, wait_job


def _job_field(show_output: str, field: str) -> int:
    match = re.search(rf"\b{field}=(\d+)", show_output)
    assert match, f"missing {field} in scontrol output:\n{show_output}"
    return int(match.group(1))


def _env_probe_script() -> str:
    """Batch script echoing the layout vars spurd injects, one per line."""
    return (
        "#!/bin/bash\n"
        'echo "host=$(hostname)"\n'
        'echo "nnodes=$SPUR_NNODES"\n'
        'echo "ntasks=$SPUR_NTASKS"\n'
        'echo "tasks_per_node=$SPUR_TASKS_PER_NODE"\n'
        'echo "cpus_on_node=$SPUR_CPUS_ON_NODE"\n'
    )


def _probe_values(content: str, key: str) -> list[str]:
    return [
        line.split("=", 1)[1].strip()
        for line in content.splitlines()
        if line.startswith(f"{key}=")
    ]


class TestSbatchTaskDefaults:
    def test_ntasks_defaults_to_one_per_node(self, multi_node_cluster):
        """-N 2 with no -n runs two tasks, not one."""
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/default-ntasks.out"
        script = cluster.write_file("default-ntasks.sh", _env_probe_script())

        job_id = parse_job_id(
            cluster.sbatch(["-J", "default-ntasks", "-N", "2", "-o", out_path, script])
        )
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _job_field(show, "NumTasks") == 2, (
            f"-N 2 must default to 2 tasks:\n{show}"
        )
        assert _job_field(show, "NumNodes") == 2, f"expected a 2-node job:\n{show}"

        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)
        content = cluster.read_output_all_nodes(out_path)
        assert set(_probe_values(content, "ntasks")) == {"2"}, (
            f"job env must report 2 tasks:\n{content}"
        )

    def test_ntasks_per_node_scales_default_task_count(self, multi_node_cluster):
        """--ntasks-per-node raises the default to nodes * K."""
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/tpn-scale.out"
        script = cluster.write_file("tpn-scale.sh", _env_probe_script())

        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J", "tpn-scale",
                    "-N", "2",
                    "--ntasks-per-node", "3",
                    "-o", out_path,
                    script,
                ]
            )
        )
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _job_field(show, "NumTasks") == 6, (
            f"-N 2 --ntasks-per-node=3 must yield 6 tasks:\n{show}"
        )

        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)
        content = cluster.read_output_all_nodes(out_path)
        assert set(_probe_values(content, "tasks_per_node")) == {"3"}, (
            f"job env must report 3 tasks per node:\n{content}"
        )

    def test_explicit_ntasks_overrides_ntasks_per_node(self, multi_node_cluster):
        """An explicit -n wins over the --ntasks-per-node derived default."""
        cluster = multi_node_cluster
        script = cluster.write_file("explicit-n.sh", "#!/bin/bash\nsleep 1\n")

        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J", "explicit-n",
                    "-N", "2",
                    "--ntasks-per-node", "4",
                    "-n", "3",
                    script,
                ]
            )
        )
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _job_field(show, "NumTasks") == 3, (
            f"explicit -n 3 must override the tasks-per-node default:\n{show}"
        )
        wait_job(cluster, job_id, timeout=90)


class TestNodeCapping:
    def test_fewer_tasks_than_nodes_caps_node_count(self, multi_node_cluster):
        """-N 2 -n 1 cannot use the second node, so the allocation shrinks."""
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/capped.out"
        script = cluster.write_file("capped.sh", _env_probe_script())

        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "capped", "-N", "2", "-n", "1", "-o", out_path, script]
            )
        )
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _job_field(show, "NumNodes") == 1, (
            f"-N 2 -n 1 must be capped to a single node:\n{show}"
        )
        assert _job_field(show, "NumTasks") == 1, f"expected a single task:\n{show}"

        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)
        content = cluster.read_output_all_nodes(out_path)
        hosts = set(_probe_values(content, "host"))
        assert len(hosts) == 1, f"capped job must run on one host, got {hosts}"
        assert set(_probe_values(content, "nnodes")) == {"1"}, (
            f"job env must reflect the capped node count:\n{content}"
        )

    def test_ntasks_per_node_pins_node_count_against_capping(self, multi_node_cluster):
        """An explicit per-node layout keeps every requested node.

        --ntasks-per-node states the layout directly, so the task count must
        not be used to shrink the allocation behind the user's back.
        """
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/pinned.out"
        script = cluster.write_file("pinned.sh", _env_probe_script())

        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J", "pinned",
                    "-N", "2",
                    "-n", "1",
                    "--ntasks-per-node", "1",
                    "-o", out_path,
                    script,
                ]
            )
        )
        assert job_id is not None

        show = cluster.scontrol("show", "job", str(job_id))
        assert _job_field(show, "NumNodes") == 2, (
            f"--ntasks-per-node must pin the node count at 2:\n{show}"
        )

        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)
        content = cluster.read_output_all_nodes(out_path)
        assert len(set(_probe_values(content, "host"))) == 2, (
            f"pinned job must run on both nodes:\n{content}"
        )

    def test_capped_job_still_allocates_cpus_per_task(self, multi_node_cluster):
        """Capping the node count must not drop the CPU allocation."""
        cluster = multi_node_cluster
        out_path = f"{cluster.remote_dir}/capped-cpus.out"
        script = cluster.write_file("capped-cpus.sh", _env_probe_script())

        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J", "capped-cpus",
                    "-N", "2",
                    "-n", "1",
                    "-c", "2",
                    "-o", out_path,
                    script,
                ]
            )
        )
        assert job_id is not None

        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)
        content = cluster.read_output_all_nodes(out_path)
        cpus = _probe_values(content, "cpus_on_node")
        assert cpus, f"job env must report CPUs on node:\n{content}"
        assert all(int(c) >= 2 for c in cpus), (
            f"one task with -c 2 needs at least 2 CPUs allocated, got {cpus}"
        )


class TestSrunTaskDefaults:
    def test_srun_ntasks_defaults_to_one_per_node(self, multi_node_cluster):
        """Standalone srun -N 2 fans out one task per node without -n."""
        cluster = multi_node_cluster
        code, out = cluster.srun_with_exit(
            ["-N", "2", "bash", "-c", 'echo "host=$(hostname)"']
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        hosts = {
            line.split("host=")[1].strip()
            for line in out.splitlines()
            if line.startswith("host=")
        }
        assert len(hosts) == 2, f"expected one task on each of 2 hosts, got {hosts}:\n{out}"

    def test_srun_ntasks_per_node_fans_out(self, multi_node_cluster):
        """srun --ntasks-per-node launches K tasks on every allocated node."""
        cluster = multi_node_cluster
        code, out = cluster.srun_with_exit(
            [
                "-N", "2",
                "--ntasks-per-node", "2",
                "bash", "-c", 'echo "rank=$SPUR_PROCID host=$(hostname)"',
            ]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        lines = [ln for ln in out.splitlines() if ln.startswith("rank=")]
        assert len(lines) == 4, f"expected 4 task lines (2 nodes x 2), got:\n{out}"

        ranks = {ln.split("rank=")[1].split()[0] for ln in lines}
        assert ranks == {"0", "1", "2", "3"}, f"expected ranks 0-3, got {ranks}:\n{out}"
