# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for a step's own share of its allocation.

A step that types ``--mem``, ``--cpus-per-task`` or a GPU count gets those as
limits of its own, written to its ``step_<n>`` leaf, while a step that types
none keeps sharing the job's. Probes read their own cgroup through
``/proc/self/cgroup``; only the agent must be root, which the ``cgroup_cluster``
and ``gpu_cluster`` fixtures provide.
"""

import re

import pytest

from cluster import parse_job_id, wait_job, wait_job_state
from cluster import (
    RESOURCE_PROBE,
    expand_cpulist,
    key_values,
    node_reserved_cpus,
    restricted_cores_config,
)

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


BUSY_NOTICE = "step creation temporarily disabled, retrying (Requested nodes are busy)"

# Marks a step's start and end in the job's log, so ordering can be read back.
_TIMED = """#!/bin/bash
echo "$1_START $(date +%s.%N)" >> "$LOG"
sleep "${2:-0}"
echo "$1_END $(date +%s.%N)" >> "$LOG"
"""


def _run_steps(cluster, name: str, sbatch_args: list[str], body: str) -> tuple[int, str, str]:
    """Run *body* as a batch job with ``$LOG``, ``$PROBE`` and ``$TIMED`` set.

    Returns the job ID, its final state and the contents of ``$LOG``.
    """
    log = f"{cluster.remote_dir}/{name}.log"
    probe = cluster.write_file(f"{name}-probe.sh", RESOURCE_PROBE)
    timed = cluster.write_file(f"{name}-timed.sh", _TIMED)
    script = cluster.write_file(
        f"{name}.sh",
        f"#!/bin/bash\nexport LOG={log} PROBE={probe} TIMED={timed}\n{body}",
    )
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-t", "5",
         "-o", f"{cluster.remote_dir}/{name}.out"] + sbatch_args + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    state = wait_job(cluster, job_id, timeout=240)
    return job_id, state, cluster.nodes[0].read_file(log)


def _stamp(log: str, mark: str) -> float:
    m = re.search(rf"^{mark} (\S+)$", log, re.MULTILINE)
    assert m, f"{mark} missing from the job log:\n{log}"
    return float(m.group(1))


def _step_state(log: str, job_id: int, step_id: int) -> str | None:
    m = re.search(rf"StepId={job_id}\.{step_id} .*State=(\S+)", log)
    return m.group(1) if m else None


def _section(log: str, tag: str) -> dict[str, str]:
    """Probe values written between ``BEGIN <tag>`` and ``END <tag>``."""
    m = re.search(rf"^BEGIN {tag}$(.*?)^END {tag}$", log, re.MULTILINE | re.DOTALL)
    assert m, f"no {tag} section in the job log:\n{log}"
    return key_values(m.group(1))


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
    # spills to swap on any host that has some and never hits its limit. A
    # step's -c becomes a cpu.max cap only with cpu_quota, as for jobs.
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"cgroup": {"constrain_swap": True, "allowed_swap_percent": 0,
                           "cpu_quota": True}}

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


class TestStepShares:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"cgroup": {"constrain_swap": True, "allowed_swap_percent": 0}}

    def test_a_step_waits_while_its_share_is_busy(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_id, state, log = _run_steps(
            cluster, "share-wait", ["-c", "2", "--mem=1G"],
            'srun --mem=768M bash "$TIMED" FIRST 8 &\n'
            'until grep -q FIRST_START "$LOG"; do sleep 0.2; done\n'
            'srun --mem=768M bash "$TIMED" SECOND 2>> "$LOG.err"\n'
            "wait\n"
            'cat "$LOG.err" >> "$LOG"\n',
        )
        assert state == "CD", f"{state}\n{cluster.debug_job(job_id)}\n{log}"
        assert f"srun: Job {job_id} {BUSY_NOTICE}" in log, log
        assert _stamp(log, "SECOND_START") >= _stamp(log, "FIRST_END"), (
            f"the second step must start only once the first frees the memory\n{log}"
        )

    def test_a_step_larger_than_its_share_is_refused_at_once(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_id = _hold_job(cluster, "share-oversize", ["--mem=1G"])
        try:
            code, out = cluster.srun_in_allocation(job_id, ["--mem=2G", "true"])
        finally:
            cluster.scancel(str(job_id))
        assert code != 0, out
        assert "the step asks for 2048 MiB on this node but its job holds 1024 MiB" in out, out
        assert BUSY_NOTICE not in out, f"a step that can never fit must not retry\n{out}"

    def test_interrupting_a_waiting_step_cancels_it(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_id, _, log = _run_steps(
            cluster, "share-sigint", ["-c", "2", "--mem=1G"],
            # Job control so the background srun keeps a default SIGINT disposition.
            "set -m\n"
            'srun --mem=1G bash "$TIMED" HOLDER 60 &\n'
            "H=$!\n"
            'until grep -q HOLDER_START "$LOG"; do sleep 0.2; done\n'
            'srun --mem=512M true 2> "$LOG.err" &\n'
            "W=$!\n"
            f'until grep -q "{BUSY_NOTICE}" "$LOG.err"; do sleep 0.2; done\n'
            "kill -INT $W\n"
            "wait $W\n"
            'echo "WAITER_RC=$?" >> "$LOG"\n'
            "for _ in $(seq 30); do\n"
            '  scontrol show step "$SLURM_JOB_ID" > "$LOG.steps"\n'
            '  grep -q "\\.1 .*State=CANCELLED" "$LOG.steps" && break\n'
            "  sleep 1\n"
            "done\n"
            'cat "$LOG.steps" >> "$LOG"\n'
            "kill $H; wait\n",
        )
        assert key_values(log).get("WAITER_RC") == "130", log
        assert _step_state(log, job_id, 1) == "CANCELLED", log

    def test_interrupting_a_racing_waiting_step_cancels_it(self, cgroup_cluster):
        # Unlike test_interrupting_a_waiting_step_cancels_it above, neither
        # step waits for the other to start: both race for the one-step
        # share, and the loser is decided by which RunStep reaches the agent
        # first, not by which CreateJobStep ran first. So the step that ends
        # up waiting here is not always the higher step id -- it can be the
        # job's very first step, id 0.
        cluster = cgroup_cluster
        job_id, _, log = _run_steps(
            cluster, "share-sigint-race", ["-c", "2", "--mem=1G"],
            # Job control so the background srun keeps a default SIGINT disposition.
            "set -m\n"
            'srun --mem=768M bash "$TIMED" A 60 2> "$LOG.a.err" &\n'
            "A=$!\n"
            'srun --mem=768M bash "$TIMED" B 60 2> "$LOG.b.err" &\n'
            "B=$!\n"
            f'until grep -q "{BUSY_NOTICE}" "$LOG.a.err" "$LOG.b.err" 2>/dev/null; do sleep 0.2; done\n'
            f'if grep -q "{BUSY_NOTICE}" "$LOG.a.err"; then W=$A; O=$B; else W=$B; O=$A; fi\n'
            "kill -INT $W\n"
            "wait $W\n"
            'echo "WAITER_RC=$?" >> "$LOG"\n'
            "for _ in $(seq 30); do\n"
            '  scontrol show step "$SLURM_JOB_ID" > "$LOG.steps"\n'
            '  grep -q "State=CANCELLED" "$LOG.steps" && break\n'
            "  sleep 1\n"
            "done\n"
            'cat "$LOG.steps" >> "$LOG"\n'
            "kill $O; wait\n",
        )
        assert key_values(log).get("WAITER_RC") == "130", log
        cancelled = re.findall(rf"StepId={job_id}\.(\d+) .*State=CANCELLED", log)
        assert len(cancelled) == 1, f"expected exactly one cancelled step\n{log}"

    def test_a_step_over_its_memory_ends_out_of_memory(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_id, state, log = _run_steps(
            cluster, "share-oom", ["--mem=512"],
            "srun --mem=64 bash -c 'a=$(head -c $((160*1024*1024)) /dev/zero | tr \"\\0\" x)'"
            ' 2>> "$LOG"\n'
            'echo "STEP_RC=$?" >> "$LOG"\n'
            'scontrol show step "$SLURM_JOB_ID" >> "$LOG"\n',
        )
        assert key_values(log).get("STEP_RC") not in (None, "0"), log
        assert (
            f"srun: error: Detected an oom_kill event in StepId={job_id}.0. "
            "Some of the step tasks have been OOM Killed."
        ) in log, log
        assert _step_state(log, job_id, 0) == "OUT_OF_MEMORY", log
        assert state == "CD", f"only the step ran out of memory, got {state}\n{log}"

    def test_omp_threads_follow_the_steps_cpus(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_id, _, log = _run_steps(
            cluster, "share-omp", ["-c", "2"],
            'echo "BEGIN TYPED" >> "$LOG"; srun -c 1 bash "$PROBE" >> "$LOG"; echo "END TYPED" >> "$LOG"\n'
            'echo "BEGIN PLAIN" >> "$LOG"; srun bash "$PROBE" >> "$LOG"; echo "END PLAIN" >> "$LOG"\n',
        )
        typed, plain = _section(log, "TYPED"), _section(log, "PLAIN")
        assert typed.get("OMP_NUM_THREADS") == "1" and typed.get("NPROC") == "1", log
        assert plain.get("OMP_NUM_THREADS") == "2", (
            f"a step that typed no CPUs keeps the job's thread count\n{log}"
        )


class TestTypedStepRefusals:
    def test_pty_warns_and_attach_refuses_typed_flags(self, cluster):
        job_id = _hold_job(cluster, "typed-refused", [])
        try:
            pty_code, pty = cluster.srun_in_allocation(job_id, ["--pty", "--mem=64", "true"])
            attach_code, attach = cluster.srun_with_exit(
                ["--jobid", str(job_id), "--overlap", "-c", "1", "true"]
            )
        finally:
            cluster.scancel(str(job_id))
        assert pty_code == 0 and (
            "a --pty step runs with its job's CPUs, memory and GPUs; "
            "-c/--mem*/--cpus-per-gpu/--gres/--gpus* are ignored"
        ) in pty, pty
        assert attach_code != 0 and "srun --jobid attach shares the job's resources" in attach, (
            attach
        )


@pytest.mark.rootful
class TestGpuStepShares:
    def test_a_gpu_step_waits_for_held_gpus(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        if cluster.node_gpu_count(cluster.node_names[0]) < 2:
            pytest.skip("need >= 2 GPUs on node 0")
        job_id, state, log = _run_steps(
            cluster, "gpu-wait", ["--gres=gpu:2"],
            'srun --gpus=2 bash "$TIMED" HOLDER 8 &\n'
            'until grep -q HOLDER_START "$LOG"; do sleep 0.2; done\n'
            'srun --gpus=1 bash "$TIMED" WAITER 2>> "$LOG.err"\n'
            "wait\n"
            'cat "$LOG.err" >> "$LOG"\n',
        )
        assert state == "CD", f"{state}\n{cluster.debug_job(job_id)}\n{log}"
        assert f"srun: Job {job_id} {BUSY_NOTICE}" in log, log
        assert _stamp(log, "WAITER_START") >= _stamp(log, "HOLDER_END"), log

    def test_an_overlap_step_runs_while_gpus_are_held(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        if cluster.node_gpu_count(cluster.node_names[0]) < 2:
            pytest.skip("need >= 2 GPUs on node 0")
        job_id, state, log = _run_steps(
            cluster, "gpu-overlap", ["--gres=gpu:2"],
            'srun --gpus=2 bash "$TIMED" HOLDER 8 &\n'
            'until grep -q HOLDER_START "$LOG"; do sleep 0.2; done\n'
            'srun --overlap --gpus=1 bash "$TIMED" OVERLAP 2>> "$LOG.err"\n'
            "wait\n"
            'cat "$LOG.err" >> "$LOG"\n',
        )
        assert state == "CD", f"{state}\n{cluster.debug_job(job_id)}\n{log}"
        assert BUSY_NOTICE not in log, log
        assert _stamp(log, "OVERLAP_END") < _stamp(log, "HOLDER_END"), (
            f"--overlap must not wait for the held GPUs\n{log}"
        )


@pytest.mark.rootful
class TestTypedGpuSteps:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes):
        return restricted_cores_config(ssh_nodes[0])

    def _reserved(self, cluster) -> tuple[set[int], int]:
        cluster.gpu_preflight(1)
        out = cluster.scontrol_show_node(cluster.node_names[0])
        gpus = cluster.node_gpu_count(cluster.node_names[0])
        if gpus < 2:
            pytest.skip("need >= 2 GPUs on node 0")
        _, reserved = node_reserved_cpus(out)
        return reserved, len(reserved) // gpus

    def test_typed_gpu_steps_are_pinned_to_their_gpus_reserved_cpus(self, gpu_cluster):
        cluster = gpu_cluster
        reserved, per_gpu_cpus = self._reserved(cluster)
        if per_gpu_cpus < 2:
            pytest.skip("need >= 2 reserved CPUs per GPU")
        step_cpus = per_gpu_cpus // 2
        job_id, state, log = _run_steps(
            cluster, "gpu-pin", ["--gres=gpu:2", "-c", str(2 * per_gpu_cpus)],
            f'srun --gpus=1 -c {step_cpus} bash -c \'bash "$PROBE"; sleep 5\' > "$LOG.a" &\n'
            f'srun --gpus=1 -c {step_cpus} bash -c \'bash "$PROBE"; sleep 5\' > "$LOG.b" &\n'
            "wait\n"
            'for s in a b; do echo "BEGIN $s"; cat "$LOG.$s"; echo "END $s"; done >> "$LOG"\n',
        )
        assert state == "CD", f"{state}\n{cluster.debug_job(job_id)}\n{log}"
        sets = [expand_cpulist(_section(log, s)["CPUS"]) for s in ("a", "b")]
        for cpus in sets:
            assert len(cpus) == step_cpus and cpus <= reserved, (
                f"a typed GPU step runs on {step_cpus} of its GPU's reserved CPUs: "
                f"{sorted(cpus)} vs reserved {sorted(reserved)}\n{log}"
            )
        assert sets[0].isdisjoint(sets[1]), f"concurrent steps, distinct CPUs: {sets}"

    def test_per_gpu_step_flags_size_the_step_from_its_own_gpus(self, gpu_cluster):
        cluster = gpu_cluster
        reserved, per_gpu_cpus = self._reserved(cluster)
        job_id, state, log = _run_steps(
            cluster, "gpu-pergpu", ["--gres=gpu:2", "-c", str(2 * per_gpu_cpus), "--mem=4G"],
            'echo "BEGIN STEP" >> "$LOG"\n'
            'srun --gpus=1 --cpus-per-gpu=2 --mem-per-gpu=1G bash "$PROBE" >> "$LOG"\n'
            'echo "END STEP" >> "$LOG"\n',
        )
        assert state == "CD", f"{state}\n{cluster.debug_job(job_id)}\n{log}"
        step = _section(log, "STEP")
        cpus = expand_cpulist(step["CPUS"])
        assert len(cpus) == 2 and cpus <= reserved, f"one GPU x 2 CPUs: {step}"
        assert step["MEM_MAX"] == str(1024 * MIB), f"one GPU x 1 GiB: {step}"
        assert step["ROCR_VISIBLE_DEVICES"] == "0", f"the step holds one GPU: {step}"
