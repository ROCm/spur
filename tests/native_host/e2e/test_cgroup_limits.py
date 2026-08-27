# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for cgroup-v2 job resource enforcement.

The limits spurd writes derive from the controller's per-node allocation rather
than from what the user asked for, so these assert on the live control files
instead of on the submitted request. Each job reads its own cgroup through
``/proc/self/cgroup``, which needs no privilege — only the agent must be root,
which the ``cgroup_cluster`` fixture enforces.
"""

from typing import NamedTuple

import pytest

from cluster import parse_job_id, wait_job

MIB = 1024 * 1024

# Pure bash on purpose: the minimal container image has no awk or cut, and the
# same probe is reused there.
_PROBE = """#!/bin/bash
CG=""
while IFS= read -r line; do
  case "$line" in 0::*) CG="${line#0::}" ;; esac
done < /proc/self/cgroup
echo "CGROUP_PATH=$CG"
B="/sys/fs/cgroup$CG"
for f in cpu.max cpuset.cpus memory.max memory.high memory.swap.max \
         memory.oom.group pids.max; do
  if [ -r "$B/$f" ]; then echo "$f=$(cat "$B/$f")"; else echo "$f=UNREADABLE"; fi
done
echo CGROUP_PROBE_OK
"""


class _Probe(NamedTuple):
    values: dict[str, str]
    job_id: int
    output: str

    def context(self) -> str:
        return f"job {self.job_id} probe output:\n{self.output}"


def _parse(output: str) -> dict[str, str]:
    values = {}
    for line in output.splitlines():
        key, sep, value = line.partition("=")
        if sep:
            values[key.strip()] = value.strip()
    return values


def _core_count(cpuset: str) -> int:
    """Number of cores in a cgroup cpuset list.

    The kernel normalises what spurd writes (``0,1,2,3``) into ranges
    (``0-3``), so both spellings have to parse.
    """
    total = 0
    for part in cpuset.split(","):
        part = part.strip()
        if not part:
            continue
        low, _, high = part.partition("-")
        total += int(high) - int(low) + 1 if high else 1
    return total


def _require_cores(cluster, needed: int) -> None:
    cores = int(cluster.nodes[0].exec("nproc").strip())
    if cores < needed:
        pytest.skip(f"need a node with at least {needed} cores (node 0 has {cores})")


def _run_probe(
    cluster, sbatch_args: list[str], name: str, *, expect_enforced: bool = True
) -> _Probe:
    """Submit the probe pinned to node 0 and return its parsed cgroup values.

    With *expect_enforced* the job is required to have landed in its own
    ``/spur/job_<id>`` cgroup. Checking that here turns "every control file
    reads UNREADABLE" into one legible failure naming the cgroup it did land
    in, which is otherwise an easy symptom to misread.
    """
    script = cluster.write_file(f"{name}.sh", _PROBE)
    out_path = f"{cluster.remote_dir}/{name}.out"
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-o", out_path]
        + sbatch_args
        + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"

    wait_job(cluster, job_id, timeout=120)
    content = cluster.wait_output(out_path, "CGROUP_PROBE_OK", timeout=120)
    assert "CGROUP_PROBE_OK" in content, (
        f"probe did not run to completion\n{cluster.debug_job(job_id)}\n"
        f"output:\n{content}"
    )

    probe = _Probe(_parse(content), job_id, content)
    if expect_enforced:
        assert probe.values.get("CGROUP_PATH") == f"/spur/job_{job_id}", (
            f"job ran outside its own cgroup, so no limit was applied to it "
            f"(agent user: {cluster.spurd_agent_user(0)!r})\n{probe.context()}\n"
            f"spurd log:\n{cluster.spurd_log(0)[-2000:]}"
        )
    return probe


class TestCgroupDefaults:
    def test_limits_match_the_node_allocation(self, cgroup_cluster):
        cluster = cgroup_cluster
        _require_cores(cluster, 2)
        probe = _run_probe(
            cluster, ["--cpus-per-task=2", "--mem=1024"], "cg-alloc"
        )
        vals = probe.values

        assert vals["memory.max"] == str(1024 * MIB), probe.context()
        # allowed_ram_percent defaults to 100, which collapses the soft ceiling
        # onto the hard one.
        assert vals["memory.high"] == vals["memory.max"], probe.context()
        assert vals["memory.swap.max"] == "0", probe.context()
        assert vals["memory.oom.group"] == "1", probe.context()
        assert _core_count(vals["cpuset.cpus"]) == 2, probe.context()
        # Slurm bounds CPU with the cpuset alone; the CFS quota is opt-in.
        assert vals["cpu.max"] == "max 100000", probe.context()

    def test_cpuset_covers_every_task_on_the_node(self, cgroup_cluster):
        # Sizing the budget from `cpus_per_task` alone would pin 2 cores here
        # instead of 4, which is the under-provisioning this closes.
        cluster = cgroup_cluster
        _require_cores(cluster, 4)
        probe = _run_probe(
            cluster,
            ["--ntasks-per-node=2", "--cpus-per-task=2", "--mem=512"],
            "cg-multitask",
        )
        assert _core_count(probe.values["cpuset.cpus"]) == 4, probe.context()

    def test_job_without_a_memory_request_is_left_unbounded(self, cgroup_cluster):
        # A job with no memory budget gets no memory ceiling — and therefore no
        # swap ceiling either. Bounding swap to 0 while RAM stays unlimited is a
        # combination Slurm never emits.
        probe = _run_probe(cgroup_cluster, ["--cpus-per-task=1"], "cg-nomem")
        vals = probe.values

        assert vals["memory.max"] == "max", probe.context()
        assert vals["memory.high"] == "max", probe.context()
        assert vals["memory.swap.max"] == "max", probe.context()

    def test_memory_ceiling_floors_at_min_ram_mb(self, cgroup_cluster):
        # Below the floor a job is OOM-killed during its own startup, before
        # anything can clean up after it.
        probe = _run_probe(
            cgroup_cluster, ["--cpus-per-task=1", "--mem=8"], "cg-tinymem"
        )
        assert probe.values["memory.max"] == str(30 * MIB), probe.context()
        assert probe.values["memory.high"] == str(30 * MIB), probe.context()

    def test_container_job_runs_inside_the_job_cgroup(self, cgroup_cluster, tmp_path):
        # Containers share the batch cgroup, so the limits come for free; the
        # container-specific risk is the pid-namespace tree escaping it. The
        # container mounts a fresh sysfs with no cgroup2 under it, so membership
        # is asserted via /proc/self/cgroup rather than by re-reading the files.
        cluster = cgroup_cluster
        cluster.container_preflight()
        image = cluster.build_container_image(tmp_path)
        probe = _run_probe(
            cluster,
            ["--cpus-per-task=1", "--mem=256", f"--container-image={image}"],
            "cg-container",
        )
        assert probe.values["CGROUP_PATH"] == f"/spur/job_{probe.job_id}", (
            probe.context()
        )


class TestCgroupCpuQuota:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"cgroup": {"cpu_quota": True}}

    def test_cpu_quota_opt_in_writes_cpu_max(self, cgroup_cluster):
        cluster = cgroup_cluster
        _require_cores(cluster, 2)
        probe = _run_probe(
            cluster, ["--cpus-per-task=2", "--mem=512"], "cg-quota"
        )
        assert probe.values["cpu.max"] == "200000 100000", probe.context()


class TestCgroupRamHeadroom:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"cgroup": {"allowed_ram_percent": 150}}

    def test_headroom_splits_the_soft_and_hard_ceilings(self, cgroup_cluster):
        # memory.high stays at the allocation so reclaim starts there, while
        # memory.max moves out to the configured headroom.
        probe = _run_probe(
            cgroup_cluster, ["--cpus-per-task=1", "--mem=1024"], "cg-headroom"
        )
        assert probe.values["memory.high"] == str(1024 * MIB), probe.context()
        assert probe.values["memory.max"] == str(1536 * MIB), probe.context()


class TestCgroupDisabled:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"cgroup": {"enabled": False}}

    def test_master_switch_creates_no_cgroup(self, cgroup_cluster):
        # The job still runs; it just inherits the agent's cgroup instead of
        # getting one of its own.
        probe = _run_probe(
            cgroup_cluster,
            ["--cpus-per-task=1", "--mem=512"],
            "cg-off",
            expect_enforced=False,
        )
        assert not probe.values["CGROUP_PATH"].startswith("/spur/job_"), (
            probe.context()
        )


class TestCgroupConfigValidation:
    def test_controller_refuses_an_out_of_range_swap_percent(self, unstarted_cluster):
        # An ignored bad value would silently hand jobs a swap ceiling larger
        # than the memory they were granted, so the daemon must refuse to start.
        cluster = unstarted_cluster
        conf = cluster.write_file(
            "bad-cgroup.conf",
            'cluster_name = "cgroup-validation"\n'
            "[cgroup]\n"
            "allowed_swap_percent = 150\n",
            executable=False,
        )
        out = cluster.nodes[0].exec_allow_fail(
            f"timeout 20 '{cluster.bin_dir}/spurctld' -f '{conf}' "
            f"--listen '[::]:16899' --state-dir '{cluster.remote_dir}/bad-state' "
            f"2>&1; echo EXIT=$?"
        )
        assert "EXIT=0" not in out, f"spurctld must not start with an invalid config:\n{out}"
        assert "allowed_swap_percent" in out, (
            f"startup failure must name the offending field:\n{out}"
        )
