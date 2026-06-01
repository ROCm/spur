# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Single-node E2E tests for the Spur scheduler."""

import time

from cluster import parse_job_id, job_state, wait_job


class TestClusterHealth:
    def test_sinfo_returns_output(self, cluster):
        out = cluster.sinfo()
        assert out.strip(), "sinfo produced no output"

    def test_all_nodes_registered_and_idle(self, cluster):
        out = cluster.sinfo()
        for name in cluster.node_names:
            assert name in out, f"node {name} not in sinfo:\n{out}"
        assert cluster._cluster_is_ready(out), (
            f"expected {len(cluster.node_names)} idle nodes, sinfo:\n{out}"
        )


class TestJobBasics:
    def test_single_node_job_completes_with_output(self, cluster):
        out_path = f"{cluster.remote_dir}/basic.out"
        script = cluster.write_script(
            "test-basic.sh",
            '#!/bin/bash\necho "hostname=$(hostname)"\n'
            'echo "SPUR_JOB_ID=${SPUR_JOB_ID}"\necho SUCCESS\n',
        )
        out = cluster.sbatch(["-J", "test-basic", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(out)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "SUCCESS" in content, f"output:\n{content}"
        assert f"SPUR_JOB_ID={job_id}" in content, f"output:\n{content}"

    def test_failed_job_state(self, cluster):
        out_path = f"{cluster.remote_dir}/fail.out"
        script = cluster.write_script(
            "test-fail.sh",
            "#!/bin/bash\necho before-failure\nexit 42\n",
        )
        sb = cluster.sbatch(["-J", "test-fail", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state == "F", f"expected failed state, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "before-failure" in content

    def test_custom_output_and_error_paths(self, cluster):
        out_path = f"{cluster.remote_dir}/custom-out.txt"
        err_path = f"{cluster.remote_dir}/custom-err.txt"
        script = cluster.write_script(
            "test-io.sh",
            "#!/bin/bash\necho stdout-line\necho stderr-line >&2\necho CUSTOM_IO_OK\n",
        )
        sb = cluster.sbatch(["-J", "test-io", "-N", "1", "-o", out_path, "-e", err_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=60)
        stdout = cluster.read_output_on_any_node(out_path)
        assert "CUSTOM_IO_OK" in stdout
        stderr = cluster.read_output_on_any_node(err_path)
        assert "stderr-line" in stderr

    def test_percent_j_output_substitution(self, cluster):
        script = cluster.write_script("test-j.sh", "#!/bin/bash\necho J_OK\n")
        pattern = f"{cluster.remote_dir}/spur-subst-%j.out"
        sb = cluster.sbatch(["-J", "test-subst", "-N", "1", "-o", pattern, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=60)
        path = f"{cluster.remote_dir}/spur-subst-{job_id}.out"
        content = cluster.read_output_on_any_node(path)
        assert "J_OK" in content


class TestJobLifecycle:
    def test_job_cancel(self, cluster):
        script = cluster.write_script("test-long.sh", "#!/bin/bash\nsleep 300\n")
        sb = cluster.sbatch(["-J", "test-cancel", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        time.sleep(3)
        cluster.scancel(str(job_id))
        time.sleep(2)

        sq = cluster.squeue_all()
        state = job_state(sq, job_id)
        assert state in ("CA", "F", None), f"expected cancelled, got {state}"

    def test_job_cancel_releases_resources(self, cluster):
        script = cluster.write_script(
            "test-cancel-res.sh",
            "#!/bin/bash\ntrap '' TERM\nsleep 300\n",
        )
        sb = cluster.sbatch(["-J", "cancel-res", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        time.sleep(4)
        cluster.scancel(str(job_id))

        deadline = time.time() + 15
        while time.time() < deadline:
            info = cluster.sinfo()
            if "idle" in info and "mix" not in info and "alloc" not in info:
                return
            time.sleep(2)
        assert False, "node should return to idle after cancel"

    def test_job_hold_and_release(self, cluster):
        out_path = f"{cluster.remote_dir}/hold.out"
        script = cluster.write_script("test-hold.sh", "#!/bin/bash\necho HOLD_OK\n")
        sb = cluster.sbatch(["-J", "test-hold", "-N", "1", "-H", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        time.sleep(2)
        sq = cluster.squeue_all()
        assert job_state(sq, job_id) == "PD"

        cluster.scontrol("release", str(job_id))
        wait_job(cluster, job_id, timeout=60)

        content = cluster.read_output_on_any_node(out_path)
        assert "HOLD_OK" in content

    def test_job_dependency_afterok(self, cluster):
        out_b = f"{cluster.remote_dir}/dep-b.out"
        script_a = cluster.write_script(
            "dep-a.sh",
            "#!/bin/bash\necho DEP_A_START\nsleep 6\necho DEP_A_DONE\n",
        )
        script_b = cluster.write_script("dep-b.sh", "#!/bin/bash\necho DEP_B_RAN\n")

        sb_a = cluster.sbatch(["-J", "dep-a", "-N", "1", script_a])
        job_a = parse_job_id(sb_a)
        assert job_a is not None

        sb_b = cluster.sbatch([
            "-J", "dep-b", "-N", "1", "-o", out_b,
            f"--dependency=afterok:{job_a}", script_b,
        ])
        job_b = parse_job_id(sb_b)
        assert job_b is not None

        time.sleep(3)
        sq = cluster.squeue_all()
        assert job_state(sq, job_a) == "R"
        assert job_state(sq, job_b) == "PD"

        wait_job(cluster, job_a, timeout=60)
        time.sleep(3)
        wait_job(cluster, job_b, timeout=60)

        content = cluster.read_output_on_any_node(out_b)
        assert "DEP_B_RAN" in content

    def test_time_limit_enforced(self, cluster):
        out_path = f"{cluster.remote_dir}/walltime.out"
        script = cluster.write_script(
            "walltime.sh",
            "#!/bin/bash\necho WALLTIME_STARTED\nsleep 300\necho WALLTIME_SHOULD_NOT_REACH\n",
        )
        sb = cluster.sbatch(["-J", "walltime", "-N", "1", "-o", out_path, "-t", "0:00:10", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=45)
        assert state in ("CA", "F", "TO", "GONE"), f"job should be killed, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "WALLTIME_STARTED" in content
        assert "WALLTIME_SHOULD_NOT_REACH" not in content

    def test_env_passthrough_export(self, cluster):
        out_path = f"{cluster.remote_dir}/env.out"
        script = cluster.write_script(
            "test-env.sh",
            '#!/bin/bash\necho "MYVAR=${MYVAR}"\necho "MULTIVAR=${MULTIVAR}"\necho ENV_OK\n',
        )
        cmd = (
            f"SPUR_CONTROLLER_ADDR='{cluster.controller_addr}' "
            f"PATH='{cluster.bin_dir}':$PATH "
            f"MYVAR=hello123 MULTIVAR=world456 "
            f"'{cluster.bin_dir}/sbatch' -J test-env -N 1 "
            f"-o '{out_path}' --export=MYVAR,MULTIVAR '{script}'"
        )
        sb = cluster.nodes[0].exec(cmd)
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=60)
        content = cluster.read_output_on_any_node(out_path)
        assert "MYVAR=hello123" in content, f"output:\n{content}"
        assert "MULTIVAR=world456" in content, f"output:\n{content}"
