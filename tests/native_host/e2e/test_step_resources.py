# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for a step's own share of its allocation.

A step that types ``--mem``, ``--cpus-per-task`` or a GPU count gets those as
limits of its own, written to its ``step_<n>`` leaf, while a step that types
none keeps sharing the job's. Probes read their own cgroup through
``/proc/self/cgroup``; only the agent must be root, which the ``cgroup_cluster``
and ``gpu_cluster`` fixtures provide.
"""

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

MIB = 1024 * 1024
KFD = "/dev/kfd"

_LEAF_PROBE = """#!/bin/bash
CG=""
while IFS= read -r line; do
  case "$line" in 0::*) CG="${line#0::}" ;; esac
done < /proc/self/cgroup
echo "CGROUP_PATH=$CG"
for f in cpu.max memory.max memory.oom.group; do
  echo "$f=$(cat "/sys/fs/cgroup$CG/$f" 2>/dev/null || echo UNREADABLE)"
done
echo LEAF_PROBE_OK
"""

_GPU_PROBE = """#!/bin/bash
openable=0
for d in /dev/dri/renderD*; do
  [ -e "$d" ] || continue
  if { : < "$d"; } 2>/dev/null; then openable=$((openable+1)); fi
done
echo "RENDER_OPENABLE=$openable"
if { : < /dev/kfd; } 2>/dev/null; then echo "KFD=OPEN"; else echo "KFD=DENIED"; fi
echo "ROCR=${ROCR_VISIBLE_DEVICES:-unset}"
echo GPU_PROBE_OK
"""


def _values(output: str) -> dict[str, str]:
    values = {}
    for line in output.splitlines():
        key, sep, value = line.partition("=")
        if sep:
            values[key.strip()] = value.strip()
    return values


def _hold_job(cluster, name: str, sbatch_args: list[str]) -> int:
    hold = cluster.write_file(f"{name}-hold.sh", "#!/bin/bash\nsleep 300\n")
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0]] + sbatch_args + [hold]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    try:
        wait_job_state(cluster, job_id, "R", timeout=120)
    except TimeoutError as e:
        raise TimeoutError(f"{e}\n{cluster.debug_job(job_id)}") from None
    return job_id


class TestStepOwnLimits:
    # memory.max is only a hard ceiling with swap bounded; otherwise the step
    # spills to swap on any host that has some and never hits its limit.
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"cgroup": {"constrain_swap": True, "allowed_swap_percent": 0}}

    def test_a_steps_own_limits_land_on_its_leaf(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_id = _hold_job(cluster, "step-own-leaf", ["-c", "2", "--mem=256"])
        probe = cluster.write_file("step-own-leaf-probe.sh", _LEAF_PROBE)
        try:
            code, out = cluster.srun_in_allocation(
                job_id, ["-c", "1", "--mem=64", "bash", probe]
            )
            shared_code, shared_out = cluster.srun_in_allocation(job_id, ["bash", probe])
        finally:
            cluster.scancel(str(job_id))

        assert code == 0 and "LEAF_PROBE_OK" in out, f"exit {code}\n{out}"
        values = _values(out)
        assert values["CGROUP_PATH"].rsplit("/", 1)[-1].startswith("step_"), out
        assert values["cpu.max"] == "100000 100000", out
        assert values["memory.max"] == str(64 * MIB), out
        assert values["memory.oom.group"] == "1", out

        assert shared_code == 0 and "LEAF_PROBE_OK" in shared_out, shared_out
        shared = _values(shared_out)
        assert shared["cpu.max"].startswith("max"), (
            f"a step that typed no limits keeps sharing the job's\n{shared_out}"
        )
        assert shared["memory.max"] == "max", shared_out

    def test_a_step_over_its_own_memory_dies_alone(self, cgroup_cluster):
        cluster = cgroup_cluster
        marker = f"{cluster.remote_dir}/step-own-oom-marker.txt"
        greedy = cluster.write_file(
            "step-own-oom-greedy.sh",
            "#!/bin/bash\n"
            f'echo STEP_STARTED >> "{marker}"\n'
            "a=$(head -c $((160*1024*1024)) /dev/zero | tr '\\0' x)\n"
            f'echo "STEP_OK len=${{#a}}" >> "{marker}"\n',
        )
        script = cluster.write_file(
            "step-own-oom.sh",
            "#!/bin/bash\n"
            f"srun --mem=64 bash {greedy}\n"
            f'echo "JOB_CONTINUED step_exit=$?" >> "{marker}"\n'
            f'srun bash -c \'echo SIBLING_RAN >> "{marker}"\'\n',
        )
        sb = cluster.sbatch(
            ["-J", "step-own-oom", "-N", "1", "-w", cluster.node_names[0], "-t", "2",
             "--mem=512", script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        state = wait_job(cluster, job_id, timeout=120)
        marker_out = cluster.nodes[0].read_file(marker)

        assert "STEP_STARTED" in marker_out, (
            f"the greedy step never launched\n{cluster.debug_job(job_id)}\n{marker_out!r}"
        )
        assert "STEP_OK" not in marker_out, (
            f"160 MiB must not fit under the step's own 64 MiB\n{marker_out!r}"
        )
        assert "JOB_CONTINUED" in marker_out and "SIBLING_RAN" in marker_out, (
            f"only the step over its own limit may die, not its job\n"
            f"{cluster.debug_job(job_id)}\n{marker_out!r}"
        )
        assert state == "CD", f"the job itself stayed within its memory, got {state}"


@pytest.mark.rootful
class TestStepOwnGpus:
    def test_a_step_reaches_only_the_gpus_it_asked_for(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        if cluster.node_gpu_count(cluster.node_names[0]) < 2:
            pytest.skip("need >= 2 GPUs on node 0 to leave one outside the step")
        if "OPEN" not in cluster.nodes[0].exec_allow_fail(
            f"if : < {KFD} 2>/dev/null; then echo OPEN; fi"
        ):
            pytest.skip(f"{KFD} is not openable outside a job, so it proves nothing here")

        job_id = _hold_job(cluster, "step-own-gpus", ["--gres=gpu:2"])
        probe = cluster.write_file("step-own-gpus-probe.sh", _GPU_PROBE)
        try:
            one_code, one = cluster.srun_in_allocation(job_id, ["--gpus=1", "bash", probe])
            none_code, none = cluster.srun_in_allocation(job_id, ["--gpus=0", "bash", probe])
            all_code, every = cluster.srun_in_allocation(job_id, ["bash", probe])
        finally:
            cluster.scancel(str(job_id))

        assert one_code == 0 and "GPU_PROBE_OK" in one, one
        assert _values(one)["RENDER_OPENABLE"] == "1", one
        assert _values(one)["KFD"] == "OPEN", one
        assert _values(one)["ROCR"] == "0", (
            f"the step can see only its own GPU, so it is numbered 0\n{one}"
        )

        assert none_code == 0 and "GPU_PROBE_OK" in none, none
        assert _values(none)["RENDER_OPENABLE"] == "0", none
        assert _values(none)["KFD"] == "DENIED", none

        assert all_code == 0 and "GPU_PROBE_OK" in every, every
        assert _values(every)["RENDER_OPENABLE"] == "2", (
            f"a step that asked for no GPUs keeps the job's\n{every}"
        )
