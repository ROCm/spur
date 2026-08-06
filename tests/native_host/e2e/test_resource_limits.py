# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for per-job resource limits.

cgroup isolation only happens when spurd runs as root, so the enforcement
tests use a rootful agent and skip without sudo or a cgroup v2 hierarchy. The
env-var side of CPU limiting works unprivileged and runs against the default
cluster.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

CGROUP_ROOT = "/sys/fs/cgroup/spur"

# `tail` buffers its whole input before writing, so this allocates ~512 MiB of
# anonymous memory using only coreutils.
MEMORY_HOG = "#!/bin/bash\nhead -c 512M /dev/zero | tail -c 512M > /dev/null\n"


@pytest.fixture
def rootful_cluster(unstarted_cluster):
    """A cluster whose agents run as root, so cgroup limits are applied."""
    cluster = unstarted_cluster
    cluster.root_agent_preflight()
    probe = cluster.nodes[0].exec_allow_fail(
        "test -f /sys/fs/cgroup/cgroup.controllers && echo V2 || echo NO"
    )
    if "V2" not in probe:
        pytest.skip("cgroup v2 unified hierarchy is not mounted")
    cluster.start(agent_as_root=True)
    return cluster


def _read_cgroup(cluster, job_id: int, name: str) -> str:
    """Read a cgroup file for a job from whichever node is running it."""
    path = f"{CGROUP_ROOT}/job_{job_id}/{name}"
    sudo = cluster._sudo_prefix()
    for node in cluster.nodes:
        out = node.exec_allow_fail(f"{sudo}cat '{path}' 2>/dev/null")
        if out.strip():
            return out.strip()
    return ""


def _wait_cgroup(cluster, job_id: int, name: str, timeout: int = 30) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        value = _read_cgroup(cluster, job_id, name)
        if value:
            return value
        time.sleep(1)
    pytest.skip(f"cgroup file {name} for job {job_id} never appeared under {CGROUP_ROOT}")


def _run_and_read_cgroup(cluster, name: str, args: list[str], cgroup_file: str) -> str:
    script = cluster.write_file(f"{name}.sh", "#!/bin/bash\nsleep 120\n")
    job_id = parse_job_id(cluster.sbatch(["-J", name] + args + [script]))
    assert job_id is not None, f"{name} was not submitted"
    wait_job_state(cluster, job_id, "R", timeout=90)
    try:
        return _wait_cgroup(cluster, job_id, cgroup_file)
    finally:
        cluster.cli_allow_fail(["scancel", str(job_id)])


class TestCgroupLimits:
    def test_mem_flag_sets_memory_max(self, rootful_cluster):
        value = _run_and_read_cgroup(
            rootful_cluster, "cg-mem", ["--mem", "512M"], "memory.max"
        )
        assert int(value) == 512 * 1024 * 1024, (
            f"memory.max should be the --mem value in bytes, got {value}"
        )

    def test_cpus_per_task_sets_cpu_max(self, rootful_cluster):
        quota, period = _run_and_read_cgroup(
            rootful_cluster, "cg-cpu", ["-c", "2"], "cpu.max"
        ).split()
        assert period == "100000", f"unexpected cpu period: {period}"
        assert int(quota) == 2 * 100_000, (
            f"cpu.max quota should scale with --cpus-per-task, got {quota}"
        )

    def test_oom_group_is_enabled(self, rootful_cluster):
        """Without oom.group the kernel kills one process and the job limps on
        with a broken task tree."""
        value = _run_and_read_cgroup(
            rootful_cluster, "cg-oomg", ["--mem", "256M"], "memory.oom.group"
        )
        assert value == "1", f"memory.oom.group should be enabled, got {value}"

    def test_pids_max_has_a_floor(self, rootful_cluster):
        value = _run_and_read_cgroup(
            rootful_cluster, "cg-pids", ["-c", "1"], "pids.max"
        )
        assert int(value) >= 1024, f"pids.max should have a floor, got {value}"

    def test_mem_per_cpu_alone_leaves_memory_unlimited(self, rootful_cluster):
        """The agent derives memory.max from --mem only; --mem-per-cpu shapes
        the allocation but not the cgroup."""
        value = _run_and_read_cgroup(
            rootful_cluster, "cg-mpc", ["--mem-per-cpu", "256M"], "memory.max"
        )
        assert value == "max", (
            f"expected an unset memory.max for a --mem-per-cpu job, got {value}"
        )

    def test_cgroup_is_removed_after_the_job_ends(self, rootful_cluster):
        script = rootful_cluster.write_file("cg-clean.sh", "#!/bin/bash\nsleep 5\n")
        job_id = parse_job_id(
            rootful_cluster.sbatch(["-J", "cg-clean", "--mem", "256M", script])
        )
        assert job_id is not None
        wait_job(rootful_cluster, job_id, timeout=90)

        sudo = rootful_cluster._sudo_prefix()
        for node in rootful_cluster.nodes:
            left = node.exec_allow_fail(
                f"{sudo}test -d '{CGROUP_ROOT}/job_{job_id}' && echo LEFT || echo GONE"
            )
            assert "LEFT" not in left, (
                f"cgroup for job {job_id} outlived the job on {node.host}"
            )


class TestOomDetection:
    def test_memory_hog_is_reported_out_of_memory(self, rootful_cluster):
        """A job that blows past --mem must land in OOM, not a generic failure
        -- users triage on that state."""
        script = rootful_cluster.write_file("oom.sh", MEMORY_HOG)
        job_id = parse_job_id(
            rootful_cluster.sbatch(["-J", "oom", "--mem", "64M", script])
        )
        assert job_id is not None

        state = wait_job(rootful_cluster, job_id, timeout=180)
        assert state == "OOM", (
            f"expected OOM, got {state}:\n"
            f"{rootful_cluster.scontrol('show', 'job', str(job_id))}"
        )

    def test_oom_reason_is_surfaced_in_scontrol(self, rootful_cluster):
        script = rootful_cluster.write_file("oom-reason.sh", MEMORY_HOG)
        job_id = parse_job_id(
            rootful_cluster.sbatch(["-J", "oom-reason", "--mem", "64M", script])
        )
        assert job_id is not None
        wait_job(rootful_cluster, job_id, timeout=180)

        detail = rootful_cluster.scontrol("show", "job", str(job_id))
        assert "OUT_OF_MEMORY" in detail or "OutOfMemory" in detail, (
            f"scontrol must explain the OOM:\n{detail}"
        )

    def test_the_same_job_completes_under_a_generous_limit(self, rootful_cluster):
        """Guards the OOM tests against a false positive: the workload itself
        is fine, it is the limit that kills it."""
        script = rootful_cluster.write_file("under-limit.sh", MEMORY_HOG)
        job_id = parse_job_id(
            rootful_cluster.sbatch(["-J", "under-limit", "--mem", "2048M", script])
        )
        assert job_id is not None
        assert wait_job(rootful_cluster, job_id, timeout=180) in ("CD", "GONE")


class TestCpuEnvironment:
    def test_cpus_per_task_sets_thread_env(self, cluster):
        """The env-var path is the only CPU limiting an unprivileged agent can
        do, so it must hold with or without cgroups."""
        out_path = f"{cluster.remote_dir}/cpu-env.out"
        script = cluster.write_file(
            "cpu-env.sh",
            "#!/bin/bash\n"
            'echo "OMP_NUM_THREADS=${OMP_NUM_THREADS}"\n'
            'echo "MKL_NUM_THREADS=${MKL_NUM_THREADS}"\n'
            'echo "OPENBLAS_NUM_THREADS=${OPENBLAS_NUM_THREADS}"\n'
            'echo "NUMEXPR_NUM_THREADS=${NUMEXPR_NUM_THREADS}"\n'
            "echo CPU_ENV_DONE\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "cpu-env", "-c", "3", "-o", out_path, script])
        )
        assert job_id is not None
        wait_job(cluster, job_id, timeout=90)

        content = cluster.read_output_on_any_node(out_path)
        assert "CPU_ENV_DONE" in content, f"job produced no output:\n{content}"
        for var in (
            "OMP_NUM_THREADS",
            "MKL_NUM_THREADS",
            "OPENBLAS_NUM_THREADS",
            "NUMEXPR_NUM_THREADS",
        ):
            assert f"{var}=3" in content, f"{var} did not track -c 3:\n{content}"

    def test_default_cpus_per_task_is_one(self, cluster):
        out_path = f"{cluster.remote_dir}/cpu-default.out"
        script = cluster.write_file(
            "cpu-default.sh",
            '#!/bin/bash\necho "OMP_NUM_THREADS=${OMP_NUM_THREADS}"\necho DONE\n',
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "cpu-default", "-o", out_path, script])
        )
        assert job_id is not None
        wait_job(cluster, job_id, timeout=90)
        assert "OMP_NUM_THREADS=1" in cluster.read_output_on_any_node(out_path)


class TestMemlockRlimit:
    def test_default_memlock_is_reported_at_startup(self, cluster):
        log = cluster.spurd_log()
        assert "memlock rlimit" in log, f"spurd must log its memlock state:\n{log}"

    def test_configured_memlock_reaches_the_job(self, unstarted_cluster):
        """RDMA and GPU-direct workloads fail obscurely when memlock is wrong,
        so the configured value must land on the job process itself.

        A 1 MiB cap is used because lowering the limit always succeeds, while
        raising it depends on the agent's inherited hard limit.
        """
        cluster = unstarted_cluster
        cluster.start(config_overrides={"rlimits": {"memlock": str(1024 * 1024)}})

        out_path = f"{cluster.remote_dir}/memlock.out"
        script = cluster.write_file(
            "memlock.sh",
            '#!/bin/bash\necho "MEMLOCK=$(ulimit -l)"\necho MEMLOCK_DONE\n',
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "memlock", "-o", out_path, script]))
        assert job_id is not None
        wait_job(cluster, job_id, timeout=90)

        content = cluster.read_output_on_any_node(out_path)
        assert "MEMLOCK_DONE" in content, f"job produced no output:\n{content}"
        match = re.search(r"MEMLOCK=(\S+)", content)
        assert match, f"could not read ulimit -l:\n{content}"
        # ulimit -l reports kibibytes.
        assert match.group(1) == "1024", (
            f"expected a 1 MiB memlock limit, got {match.group(1)} KB"
        )

    def test_inherit_leaves_the_agent_limit_alone(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(config_overrides={"rlimits": {"memlock": "inherit"}})

        log = cluster.spurd_log()
        assert "memlock rlimit" in log, f"spurd must log its memlock state:\n{log}"
        assert "inherit" in log, f"the configured mode must be logged:\n{log}"
