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

from cluster import job_node_indices, parse_job_id, wait_job, wait_job_state

CGROUP_ROOT = "/sys/fs/cgroup/spur"

# `tail` buffers its whole input before writing, so this allocates ~512 MiB of
# anonymous memory using only coreutils.
MEMORY_HOG = "#!/bin/bash\nhead -c 512M /dev/zero | tail -c 512M > /dev/null\n"


def _memory_controller_preflight(cluster):
    """Skip unless a job cgroup would really get a memory limit.

    spurd enables the controller on its own cgroup root, which only takes
    effect if the parent delegated `memory` down to it. Containerised hosts
    frequently have not, and spurd then logs a warning and runs the job with no
    limit at all -- so a --mem test would quietly measure nothing.
    """
    probe = f"{CGROUP_ROOT}/e2e-memory-probe"
    for i, node in enumerate(cluster.nodes):
        out = node.exec_allow_fail(
            f"{cluster._sudo_prefix()}bash -c 'mkdir -p {CGROUP_ROOT} && "
            f"echo +memory > {CGROUP_ROOT}/cgroup.subtree_control 2>/dev/null; "
            f"mkdir -p {probe} && test -f {probe}/memory.max && echo ENFORCED; "
            f"rmdir {probe} 2>/dev/null'"
        )
        if "ENFORCED" not in out:
            pytest.skip(
                f"cgroup v2 memory controller is not delegated to "
                f"{CGROUP_ROOT} on {cluster.node_names[i]}, so --mem cannot "
                f"be enforced there"
            )


def _start_rootful(cluster, **start_kwargs):
    """Start *cluster* with root agents, so cgroup limits are applied."""
    cluster.root_agent_preflight()
    probe = cluster.nodes[0].exec_allow_fail(
        "test -f /sys/fs/cgroup/cgroup.controllers && echo V2 || echo NO"
    )
    if "V2" not in probe:
        pytest.skip("cgroup v2 unified hierarchy is not mounted")
    _memory_controller_preflight(cluster)
    # Job ids restart at 1 for every cluster while cgroup paths are global to
    # the host, so a leftover job_N from an earlier test reads as this one's.
    for node in cluster.nodes:
        node.exec_allow_fail(
            f"{cluster._sudo_prefix()}bash -c 'for cg in {CGROUP_ROOT}/job_*; do "
            f'[ -d "$cg" ] || continue; echo 1 > "$cg/cgroup.kill" 2>/dev/null; '
            f"rmdir \"$cg\" 2>/dev/null; done'"
        )
    cluster.start(agent_as_root=True, **start_kwargs)
    return cluster


@pytest.fixture
def rootful_cluster(unstarted_cluster):
    return _start_rootful(unstarted_cluster)


@pytest.fixture
def swap_constrained_cluster(unstarted_cluster):
    """A rootful cluster that also caps swap."""
    return _start_rootful(
        unstarted_cluster,
        config_overrides={"cgroup": {"constrain_swap_space": True}},
    )


def _job_node(cluster, job_id: int):
    """The node running *job_id*.

    Reading whichever node happens to hold job_N's cgroup picks up an earlier
    test's leftovers as readily as this job's own.
    """
    return cluster.nodes[job_node_indices(cluster, job_id)[0]]


def _read_cgroup(cluster, node, job_id: int, name: str) -> str:
    path = f"{CGROUP_ROOT}/job_{job_id}/{name}"
    out = node.exec_allow_fail(f"{cluster._sudo_prefix()}cat '{path}' 2>/dev/null")
    return out.strip()


def _wait_cgroup(cluster, node, job_id: int, name: str, timeout: int = 30) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        value = _read_cgroup(cluster, node, job_id, name)
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
        return _wait_cgroup(cluster, _job_node(cluster, job_id), job_id, cgroup_file)
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

    def test_swap_is_left_alone_by_default(self, rootful_cluster):
        """Capping swap kills jobs that used to survive on a swap-backed node,
        so it stays opt-in."""
        value = _run_and_read_cgroup(
            rootful_cluster, "cg-swap-off", ["--mem", "512M"], "memory.swap.max"
        )
        assert value == "max", (
            f"expected an untouched memory.swap.max without "
            f"cgroup.constrain_swap_space, got {value}"
        )

    def test_constrain_swap_space_denies_swap(self, swap_constrained_cluster):
        value = _run_and_read_cgroup(
            swap_constrained_cluster, "cg-swap-on", ["--mem", "512M"], "memory.swap.max"
        )
        assert int(value) == 0, (
            f"constrain_swap_space with the default allowance should deny swap "
            f"outright, got {value}"
        )

    def test_allowed_swap_space_scales_with_the_allocation(self, unstarted_cluster):
        cluster = _start_rootful(
            unstarted_cluster,
            config_overrides={
                "cgroup": {"constrain_swap_space": True, "allowed_swap_space": 50}
            },
        )
        value = _run_and_read_cgroup(
            cluster, "cg-swap-pct", ["--mem", "512M"], "memory.swap.max"
        )
        assert int(value) == 256 * 1024 * 1024, (
            f"memory.swap.max should be 50% of --mem, got {value}"
        )

    def test_cgroup_is_removed_after_the_job_ends(self, rootful_cluster):
        script = rootful_cluster.write_file("cg-clean.sh", "#!/bin/bash\nsleep 5\n")
        job_id = parse_job_id(
            rootful_cluster.sbatch(["-J", "cg-clean", "--mem", "256M", script])
        )
        assert job_id is not None
        wait_job_state(rootful_cluster, job_id, "R", timeout=90)
        node = _job_node(rootful_cluster, job_id)
        wait_job(rootful_cluster, job_id, timeout=90)

        left = node.exec_allow_fail(
            f"{rootful_cluster._sudo_prefix()}test -d '{CGROUP_ROOT}/job_{job_id}' "
            f"&& echo LEFT || echo GONE"
        )
        assert "LEFT" not in left, (
            f"cgroup for job {job_id} outlived the job on {node.host}"
        )


class TestOomDetection:
    """OOM only happens once swap is capped: with swap available the kernel
    pages the overflow out and the job runs to completion over its --mem."""

    def test_memory_hog_is_reported_out_of_memory(self, swap_constrained_cluster):
        """A job that blows past --mem must land in OOM, not a generic failure
        -- users triage on that state."""
        cluster = swap_constrained_cluster
        script = cluster.write_file("oom.sh", MEMORY_HOG)
        job_id = parse_job_id(cluster.sbatch(["-J", "oom", "--mem", "64M", script]))
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=180)
        assert state == "OOM", (
            f"expected OOM, got {state}:\n"
            f"{cluster.scontrol('show', 'job', str(job_id))}"
        )

    def test_oom_reason_is_surfaced_in_scontrol(self, swap_constrained_cluster):
        cluster = swap_constrained_cluster
        script = cluster.write_file("oom-reason.sh", MEMORY_HOG)
        job_id = parse_job_id(
            cluster.sbatch(["-J", "oom-reason", "--mem", "64M", script])
        )
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)

        detail = cluster.scontrol("show", "job", str(job_id))
        assert "OUT_OF_MEMORY" in detail or "OutOfMemory" in detail, (
            f"scontrol must explain the OOM:\n{detail}"
        )

    def test_the_same_job_completes_under_a_generous_limit(
        self, swap_constrained_cluster
    ):
        """Guards the OOM tests against a false positive: the workload itself
        is fine, it is the limit that kills it."""
        cluster = swap_constrained_cluster
        script = cluster.write_file("under-limit.sh", MEMORY_HOG)
        job_id = parse_job_id(
            cluster.sbatch(["-J", "under-limit", "--mem", "2048M", script])
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=180) in ("CD", "GONE")


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
