# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for core-layout changes under running jobs.

A restart-only change to a node's core layout (spec cores, reserved memory,
reserved cores per GPU, a GPU's ``cores=``) must leave running jobs the CPUs
and memory limits they had, make queued GPU jobs wait for reserved CPUs that
older jobs still hold instead of spending their requeue budget, and move no
GPU's reserved cores but those of the GPU whose inputs changed. Expectations
come from the node itself, never from hardcoded CPU IDs.
"""

import copy
import re
import time

import pytest

from cluster import (
    RESOURCE_PROBE,
    expand_cpulist,
    job_state,
    key_values,
    node_reserved_cpus,
    parse_job_id,
    restricted_cores_config,
    threads_per_core,
    wait_job,
    wait_job_state,
    wait_until,
)
from conftest import _deploy_cluster

MIB = 1024 * 1024
SPEC_MEMORY_MB = 2048
MORE_SPEC_MEMORY_MB = 3072
# spurd's fixed heartbeat period: the controller asks it to re-register for a
# changed layout on the first heartbeat after the change.
HEARTBEAT_SECS = 30
CONVERGE_SECS = 3 * HEARTBEAT_SECS
WAIT_WINDOW_SECS = 60
REREGISTER = "controller requested re-registration"
# With a budget of one, a single budget-spending refusal shows at once as
# Restarts=1 and a hold, instead of after several doubling backoffs.
FAST_REQUEUE = {"controller": {"max_batch_requeue": 1, "dispatch_reject_cooldown_secs": 1}}

_TICKER = """#!/bin/bash
while :; do
  echo "CPUS=$(awk '/^Cpus_allowed_list:/ {print $2}' /proc/self/status)"
  sleep 5
done
"""

_RENDER_PROBE = r"""R=""
for d in /dev/dri/renderD*; do
  [ -e "$d" ] || continue
  if { : < "$d"; } 2>/dev/null; then R="$R ${d#/dev/dri/renderD}"; fi
done
echo "RENDERS=${R# }"
echo RENDER_PROBE_OK
"""

_RESET_ROOT = (
    "sudo sh -c 'if [ -d /sys/fs/cgroup/spur ]; then "
    "echo > /sys/fs/cgroup/spur/cpuset.cpus 2>/dev/null; "
    "echo max > /sys/fs/cgroup/spur/memory.high 2>/dev/null; "
    "echo max > /sys/fs/cgroup/spur/memory.max 2>/dev/null; fi'"
)


def _spec_nodes(count: int, memory_mb: int) -> dict:
    return {"nodes": [{"names": "ALL", "core_spec_count": count, "reserved_memory_mb": memory_mb}]}


def _restricted(per_gpu: int) -> dict:
    return {**FAST_REQUEUE, "nodes": [{"names": "ALL", "restricted_cores_per_gpu": per_gpu}]}


def _reserve_steps(node) -> tuple[int, int]:
    """(R before, R after) for a change that grows every GPU's reserved cores;
    the larger value still leaves a general pool."""
    after = restricted_cores_config(node)["nodes"][0]["restricted_cores_per_gpu"]
    if after < 2:
        pytest.skip(f"restricted_cores_per_gpu={after} leaves no room to grow it")
    return after // 2, after


def _node_field(cluster, name: str) -> str:
    out = cluster.scontrol_show_node(cluster.node_names[0])
    m = re.search(rf"{name}=(\S+)", out)
    assert m, f"{name} not shown:\n{out}"
    return m.group(1)


def _reserved(cluster) -> tuple[int, set[int]]:
    return node_reserved_cpus(cluster.scontrol_show_node(cluster.node_names[0]))


def _root(cluster, name: str) -> str:
    return cluster.nodes[0].exec_allow_fail(f"cat /sys/fs/cgroup/spur/{name}").strip()


def _root_memory(cluster) -> tuple[str, str]:
    return _root(cluster, "memory.high"), _root(cluster, "memory.max")


def _target_bytes(cluster, reserved_mb: int) -> str:
    total_kb = int(cluster.nodes[0].exec("awk '/^MemTotal:/ {print $2}' /proc/meminfo").strip())
    return str((total_kb // 1024 - reserved_mb) * MIB)


def _eventually(predicate, why: str, detail) -> None:
    try:
        wait_until(predicate, timeout_s=CONVERGE_SECS, interval_s=2, desc=why, tolerate=(Exception,))
    except TimeoutError:
        pytest.fail(f"{why}: {detail()}")


def _wait_root_memory(cluster, value: str, why: str) -> None:
    _eventually(
        lambda: _root_memory(cluster) == (value, value), why,
        lambda: f"memory.high/memory.max = {_root_memory(cluster)}, expected {value}",
    )


def _job_cpuset(cluster, job_id: int) -> set[int]:
    node = cluster.nodes[0]
    dirs = node.exec_allow_fail(f"ls -d /sys/fs/cgroup/spur/job_{job_id}_* 2>/dev/null").split()
    assert len(dirs) == 1, f"expected one cgroup for job {job_id}, found {dirs}"
    return expand_cpulist(node.exec(f"cat '{dirs[0]}/cpuset.cpus'").strip())


def _assert_running(cluster, job_id: int, why: str) -> None:
    assert job_state(cluster.squeue_all(), job_id) == "R", f"{why}\n{cluster.debug_job(job_id)}"


def _start_ticker(cluster, name: str, sbatch_args: list[str]) -> tuple[int, str]:
    """A job printing its own CPU affinity every 5 s, from a fresh process each
    tick, so a narrowed cpuset shows in its output."""
    script = cluster.write_file(f"{name}.sh", _TICKER)
    out_path = f"{cluster.remote_dir}/{name}.out"
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-t", "30", "-o", out_path]
        + sbatch_args + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    content = cluster.wait_output(out_path, "CPUS=", timeout=120)
    assert "CPUS=" in content, f"ticker never ran\n{cluster.debug_job(job_id)}"
    return job_id, out_path


def _ticked_cpus(cluster, out_path: str) -> set[int]:
    return expand_cpulist(key_values(cluster.nodes[0].read_file(out_path))["CPUS"])


def _probe_cpus(cluster, name: str, sbatch_args: list[str]) -> set[int]:
    """CPUs a short probe job ran on; the job must complete."""
    script = cluster.write_file(f"{name}.sh", RESOURCE_PROBE)
    out_path = f"{cluster.remote_dir}/{name}.out"
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-o", out_path] + sbatch_args + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    state = wait_job(cluster, job_id, timeout=180)
    content = cluster.wait_output(out_path, "RESOURCE_PROBE_OK", timeout=30)
    assert state == "CD" and "RESOURCE_PROBE_OK" in content, (
        f"probe ended {state}\n{cluster.debug_job(job_id)}\n{content}"
    )
    return expand_cpulist(key_values(content)["CPUS"])


def _restart_controller_with(cluster, overrides: dict) -> int:
    """Restart spurctld alone on a rewritten spur.conf; agents keep running.
    Returns the agent's re-registration count from before, for _wait_adopted."""
    before = cluster.spurd_log(0).count(REREGISTER)
    cluster.start_controller(overrides)
    wait_until(
        lambda: cluster.node_names[0] in cluster.sinfo_nodes(),
        timeout_s=60, interval_s=2, desc="controller recovery after restart", tolerate=(Exception,),
    )
    return before


def _wait_adopted(cluster, before: int) -> None:
    """spurd logs nothing on adoption itself: wait for the re-registration the
    controller asks for, then for the heartbeat that applies the new layout."""
    wait_until(
        lambda: cluster.spurd_log(0).count(REREGISTER) > before,
        timeout_s=CONVERGE_SECS, interval_s=2,
        desc="spurd re-registering for the changed core layout", tolerate=(Exception,),
    )
    time.sleep(10)


@pytest.fixture
def spec_fence_cluster(ssh_nodes, remote_bin_dir):
    """Rootful cluster with one spec core and some reserved memory per node."""
    if len(ssh_nodes) < 1:
        pytest.skip("layout-change tests require at least one node in SPUR_TEST_NODES")
    node = ssh_nodes[0]
    fstype = node.exec_allow_fail("stat -fc %T /sys/fs/cgroup").strip()
    if "cgroup2fs" not in fstype:
        pytest.skip(f"node 0 is not running cgroup v2 (/sys/fs/cgroup is {fstype!r})")
    cores = int(node.exec("lscpu -p=CORE,SOCKET | grep -v '^#' | sort -u | wc -l").strip())
    if cores < 3:
        pytest.skip(f"growing the spec onto a job needs >= 3 cores (node 0 has {cores})")
    total_mb = int(node.exec("awk '/^MemTotal:/ {print $2}' /proc/meminfo").strip()) // 1024
    if total_mb < 2 * MORE_SPEC_MEMORY_MB:
        pytest.skip(f"need >= {2 * MORE_SPEC_MEMORY_MB} MiB on node 0 (has {total_mb})")

    c = _deploy_cluster(
        ssh_nodes, remote_bin_dir, agent_as_root=True,
        config_overrides=_spec_nodes(1, SPEC_MEMORY_MB),
    )
    try:
        assert c.spurd_agent_user(0) == "root", "spec enforcement needs a rootful agent"
        yield c
    finally:
        c.teardown()
        # The `spur` root outlives spurd; a leftover reservation would leak
        # into other tests on this node.
        for n in c.nodes:
            n.exec_allow_fail(_RESET_ROOT)


class TestRunningJobsKeepTheirResources:
    def test_running_job_keeps_its_cpuset_across_a_layout_change(self, spec_fence_cluster):
        cluster = spec_fence_cluster
        old_spec = expand_cpulist(_node_field(cluster, "CPUSpecList"))
        efctv = int(_node_field(cluster, "CPUEfctv"))
        job_id, out_path = _start_ticker(cluster, "fence-cpus", ["-n", str(efctv)])
        try:
            held = _job_cpuset(cluster, job_id)
            before = _restart_controller_with(cluster, _spec_nodes(2, SPEC_MEMORY_MB))
            new_spec = expand_cpulist(_node_field(cluster, "CPUSpecList"))
            assert new_spec > old_spec and new_spec & held, (
                f"the grown spec {sorted(new_spec)} must cover CPUs the job holds {sorted(held)}"
            )
            _wait_adopted(cluster, before)
            for wait in (0, HEARTBEAT_SECS):
                time.sleep(wait)
                _assert_running(cluster, job_id, "the job outlives the layout change")
                assert _job_cpuset(cluster, job_id) == held
                assert _ticked_cpus(cluster, out_path) == held, "the job's own affinity must not narrow"
                root = expand_cpulist(_root(cluster, "cpuset.cpus"))
                assert held <= root, f"the root {sorted(root)} must keep the job's CPUs {sorted(held)}"
        finally:
            cluster.scancel(job_id)
        wait_job(cluster, job_id, timeout=120)

        _eventually(
            lambda: bool(_root(cluster, "cpuset.cpus"))
            and expand_cpulist(_root(cluster, "cpuset.cpus")).isdisjoint(new_spec),
            "the root drops the new spec CPUs once the job ends",
            lambda: f"cpuset.cpus={_root(cluster, 'cpuset.cpus')!r}, new spec {sorted(new_spec)}",
        )
        after = _probe_cpus(cluster, "fence-cpus-after", ["-n", "1"])
        assert after and after.isdisjoint(new_spec), f"a new job runs off the new spec CPUs: {sorted(after)}"

    def test_memory_high_is_held_while_an_old_layout_job_runs(self, spec_fence_cluster):
        cluster = spec_fence_cluster
        old = _target_bytes(cluster, SPEC_MEMORY_MB)
        new = _target_bytes(cluster, MORE_SPEC_MEMORY_MB)
        _wait_root_memory(cluster, old, "an idle node hardens its reservation")

        job_id, _ = _start_ticker(cluster, "fence-mem", ["-n", "1"])
        try:
            before = _restart_controller_with(cluster, _spec_nodes(1, MORE_SPEC_MEMORY_MB))
            _wait_adopted(cluster, before)
            for wait in (0, HEARTBEAT_SECS):
                time.sleep(wait)
                _assert_running(cluster, job_id, "the job outlives the layout change")
                assert _root_memory(cluster) == (old, old), (
                    "a larger reservation must not tighten the root under a job from before it"
                )
        finally:
            cluster.scancel(job_id)
        wait_job(cluster, job_id, timeout=120)
        _wait_root_memory(cluster, new, "the larger reservation applies once that job ends")

        job_id, _ = _start_ticker(cluster, "fence-mem-loosen", ["-n", "1"])
        try:
            before = _restart_controller_with(cluster, _spec_nodes(1, SPEC_MEMORY_MB))
            _wait_adopted(cluster, before)
            _wait_root_memory(cluster, old, "a smaller reservation loosens the root at once")
            _assert_running(cluster, job_id, "the job outlives the layout change")
        finally:
            cluster.scancel(job_id)
        wait_job(cluster, job_id, timeout=120)

    def test_spurd_restart_under_a_changed_layout_keeps_jobs_whole(self, spec_fence_cluster):
        cluster = spec_fence_cluster
        old = _target_bytes(cluster, SPEC_MEMORY_MB)
        new = _target_bytes(cluster, MORE_SPEC_MEMORY_MB)
        _wait_root_memory(cluster, old, "an idle node hardens its reservation")
        efctv = int(_node_field(cluster, "CPUEfctv"))
        job_id, out_path = _start_ticker(cluster, "fence-restart", ["-n", str(efctv)])
        try:
            held = _job_cpuset(cluster, job_id)
            _restart_controller_with(cluster, _spec_nodes(2, MORE_SPEC_MEMORY_MB))
            new_spec = expand_cpulist(_node_field(cluster, "CPUSpecList"))
            assert new_spec & held, f"the grown spec {sorted(new_spec)} must cover the job's {sorted(held)}"
            # A fresh spurd has no floor of its own and must take it from the
            # root as it finds it.
            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)
            for wait in (10, HEARTBEAT_SECS):
                time.sleep(wait)
                _assert_running(cluster, job_id, "the job outlives the agent restart")
                assert _job_cpuset(cluster, job_id) == held
                assert _ticked_cpus(cluster, out_path) == held, "the job's own affinity must not narrow"
                assert held <= expand_cpulist(_root(cluster, "cpuset.cpus"))
                assert _root_memory(cluster) == (old, old), "the restarted agent keeps the root's memory floor"
        finally:
            cluster.scancel(job_id)
        wait_job(cluster, job_id, timeout=120)
        _eventually(
            lambda: expand_cpulist(_root(cluster, "cpuset.cpus")).isdisjoint(new_spec)
            and _root_memory(cluster) == (new, new),
            "the root converges to the new layout once the job ends",
            lambda: f"cpuset.cpus={_root(cluster, 'cpuset.cpus')!r} memory={_root_memory(cluster)}",
        )


def _block_gpus(cluster) -> tuple[int, set[int], set[int]]:
    """Place a CPU-only job at the smaller R that fills all but one CPU of the
    larger R's general pool, then restart the controller at the larger R: the
    job now sits on CPUs that became GPUs' reserved ones, and one general CPU
    stays free. Returns (job, its CPUs, the new reserved CPUs)."""
    node = cluster.nodes[0]
    name = cluster.node_names[0]
    small, large = _reserve_steps(node)
    _, reserved_small = _reserved(cluster)
    grow = cluster.node_gpu_count(name) * (large - small) * threads_per_core(node)
    general_large = int(_node_field(cluster, "CPUEfctv")) - len(reserved_small) - grow
    if general_large < 2:
        pytest.skip(f"R={large} leaves {general_large} general CPUs; need 2")
    hold = cluster.write_file("fence-cpu-hold.sh", "#!/bin/bash\nsleep 1800\n")
    sb = cluster.sbatch(
        ["-J", "fence-cpu-hold", "-N", "1", "-w", name, "-t", "30", "-n", str(general_large - 1), hold]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    wait_job_state(cluster, job_id, "R", timeout=120)
    held = _job_cpuset(cluster, job_id)

    before = _restart_controller_with(cluster, _restricted(large))
    per_gpu, reserved = _reserved(cluster)
    assert per_gpu == large, cluster.scontrol_show_node(name)
    assert held & reserved, (
        f"the CPU-only job {sorted(held)} must sit on CPUs now reserved {sorted(reserved)}"
    )
    _wait_adopted(cluster, before)
    return job_id, held, reserved


def _submit_all_gpus(cluster, name: str) -> tuple[int, str]:
    """A job needing every GPU of node 0, so the blocked ones too."""
    node = cluster.node_names[0]
    script = cluster.write_file(f"{name}.sh", RESOURCE_PROBE)
    out_path = f"{cluster.remote_dir}/{name}.out"
    gpus = cluster.node_gpu_count(node)
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", node, "-o", out_path, f"--gres=gpu:{gpus}", "--cpus-per-gpu=1", script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    return job_id, out_path


def _restarts(detail: str) -> int:
    m = re.search(r"Restarts=(\d+)", detail)
    assert m, detail
    return int(m.group(1))


def _assert_waits(cluster, job_id: int, why: str) -> None:
    """Pending, with no requeue spent and no max-requeue hold, for the window."""
    deadline = time.time() + WAIT_WINDOW_SECS
    detail = ""
    while time.time() < deadline:
        detail = cluster.scontrol("show", "job", str(job_id))
        assert (
            "JobState=PENDING" in detail and _restarts(detail) == 0 and "JobHoldMaxRequeue" not in detail
        ), f"{why}\n{cluster.debug_job(job_id)}"
        time.sleep(3)
    assert re.search(r"Reason=Resources\b", detail), f"{why}: expected Reason=Resources\n{detail}"


def _assert_runs_on_reserved(cluster, job_id: int, out_path: str, reserved: set[int]) -> None:
    state = wait_job(cluster, job_id, timeout=CONVERGE_SECS + 60)
    content = cluster.wait_output(out_path, "RESOURCE_PROBE_OK", timeout=30)
    assert state == "CD" and "RESOURCE_PROBE_OK" in content, (
        f"the GPU job must run once the CPU-only job ends; it ended {state}\n"
        f"{cluster.debug_job(job_id)}\n{content}"
    )
    cpus = expand_cpulist(key_values(content)["CPUS"])
    assert cpus and cpus <= reserved, f"GPU job CPUs {sorted(cpus)} must be reserved ones {sorted(reserved)}"
    assert _restarts(cluster.scontrol("show", "job", str(job_id))) == 0


@pytest.mark.rootful
class TestGpuJobsWaitOutALayoutChange:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes):
        small, _ = _reserve_steps(ssh_nodes[0])
        return _restricted(small)

    def test_gpu_job_waits_for_an_old_layout_job_instead_of_hitting_max_requeue(self, gpu_cluster):
        cluster = gpu_cluster
        blocker, held, reserved = _block_gpus(cluster)
        gpu_job, out_path = _submit_all_gpus(cluster, "fence-gpu")
        try:
            _assert_waits(cluster, gpu_job, "a GPU job must wait for reserved CPUs an older job holds")
            cpus = _probe_cpus(cluster, "fence-cpu-only", ["-n", "1"])
            assert cpus.isdisjoint(reserved), f"CPU-only jobs still run, off reserved CPUs: {sorted(cpus)}"
            # A restarted controller knows no blocked CPUs until the next
            # heartbeat; a launch it makes meanwhile must not spend the budget.
            cluster.restart_controller()
            _assert_waits(cluster, gpu_job, "the wait survives a controller restart")
            _assert_running(cluster, blocker, "the older job keeps running")
            assert _job_cpuset(cluster, blocker) == held
        finally:
            cluster.scancel(blocker)
        _assert_runs_on_reserved(cluster, gpu_job, out_path, reserved)

    def test_spurd_restart_under_a_changed_layout_keeps_the_gpu_job_waiting(self, gpu_cluster):
        cluster = gpu_cluster
        blocker, held, reserved = _block_gpus(cluster)
        gpu_job, out_path = _submit_all_gpus(cluster, "fence-gpu-restart")
        try:
            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)
            _assert_waits(cluster, gpu_job, "a restarted spurd reports the blocked CPUs again after replay")
            _assert_running(cluster, blocker, "the older job survives the agent restart")
            assert _job_cpuset(cluster, blocker) == held
        finally:
            cluster.scancel(blocker)
        _assert_runs_on_reserved(cluster, gpu_job, out_path, reserved)


def _core_indices(node) -> dict[int, int]:
    """CPU -> the core index ``cores=`` takes: cores ordered socket-major, then
    by their lowest CPU ID."""
    cores: dict[tuple[int, int], list[int]] = {}
    for line in node.exec("lscpu -p=CPU,CORE,SOCKET | grep -v '^#'").split():
        cpu, core, socket = (int(field) for field in line.split(","))
        cores.setdefault((socket, core), []).append(cpu)
    ordered = sorted(cores.items(), key=lambda item: (item[0][0], min(item[1])))
    return {cpu: index for index, (_, cpus) in enumerate(ordered) for cpu in cpus}


def _hold_every_gpu(cluster, share: int, tag: str) -> dict[str, tuple[int, int, set[int]]]:
    """One gpu:1 job per GPU, each sized to its GPU's whole reserved share so it
    runs on exactly those CPUs. Returns GPU ordinal -> (job, render minor,
    CPUs). The caller cancels the jobs."""
    name = cluster.node_names[0]
    script = cluster.write_file(f"{tag}.sh", RESOURCE_PROBE + _RENDER_PROBE + "sleep 600\n")
    started = []
    for i in range(cluster.node_gpu_count(name)):
        out_path = f"{cluster.remote_dir}/{tag}-{i}.out"
        sb = cluster.sbatch(
            ["-J", f"{tag}-{i}", "-N", "1", "-w", name, "-o", out_path, "--gres=gpu:1", "-c", str(share), script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        started.append((job_id, out_path))
    held = {}
    for job_id, out_path in started:
        content = cluster.wait_output(out_path, "RENDER_PROBE_OK", timeout=180)
        assert "RENDER_PROBE_OK" in content, f"{cluster.debug_job(job_id)}\n{content}"
        values = key_values(content)
        renders = values["RENDERS"].split()
        assert len(renders) == 1, f"a gpu:1 job opens exactly its own render node: {values}"
        held[values["SPUR_JOB_GPUS"]] = (job_id, int(renders[0]), expand_cpulist(values["CPUS"]))
    assert len(held) == len(started), f"every job holds a GPU of its own: {held}"
    return held


def _gpu_picks(cluster, share: int, tag: str) -> dict[str, set[int]]:
    held = _hold_every_gpu(cluster, share, tag)
    for job_id, _, _ in held.values():
        cluster.scancel(job_id)
    for job_id, _, _ in held.values():
        wait_job(cluster, job_id, timeout=120)
    return {ordinal: cpus for ordinal, (_, _, cpus) in held.items()}


@pytest.mark.rootful
class TestReservedCoresAreSticky:
    @pytest.fixture
    def cluster_config_overrides(self, ssh_nodes):
        return restricted_cores_config(ssh_nodes[0])

    def test_other_gpus_keep_their_reserved_cpus_when_one_gpus_cores_change(
        self, gpu_cluster, cluster_config_overrides
    ):
        cluster = gpu_cluster
        node = cluster.nodes[0]
        per_gpu, reserved = _reserved(cluster)
        gpus = cluster.node_gpu_count(cluster.node_names[0])
        if gpus < 2:
            pytest.skip(f"need >= 2 GPUs on node 0 (got {gpus})")
        share = len(reserved) // gpus
        holders = _hold_every_gpu(cluster, share, "pick-before")
        before = {ordinal: cpus for ordinal, (_, _, cpus) in holders.items()}
        first, second = sorted(before, key=int)[:2]
        index = _core_indices(node)
        general = sorted(set(index.values()) - {index[cpu] for cpu in reserved})
        if len(general) < per_gpu:
            pytest.skip(f"{len(general)} general cores cannot take {per_gpu} reserved ones")
        # The highest general cores sit above every reserved core, so the fresh
        # lowest-first rule would hand `second`'s cores to `first` instead.
        top = set(general[-per_gpu:])
        moved_to = {cpu for cpu, core in index.items() if core in top}
        overrides = copy.deepcopy(cluster_config_overrides)
        overrides["devices"] = {"gres": [{
            "name": "gpu",
            "file": f"/dev/dri/renderD{holders[first][1]}",
            "cores": ",".join(str(core) for core in sorted({index[cpu] for cpu in before[second]} | top)),
        }]}
        expected = (reserved - before[first]) | moved_to
        first_job = holders.pop(first)[0]
        try:
            cluster.scancel(first_job)
            wait_job(cluster, first_job, timeout=120)
            # start_controller rewrites spur.conf on every node; the agent reads
            # [devices] only at startup.
            _restart_controller_with(cluster, overrides)
            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)
            _eventually(
                lambda: _reserved(cluster)[1] == expected,
                "only the changed GPU's reserved CPUs move",
                lambda: cluster.scontrol_show_node(cluster.node_names[0]),
            )
            for ordinal, (job_id, _, cpus) in holders.items():
                _assert_running(cluster, job_id, f"GPU {ordinal}'s job outlives the agent restart")
                assert _job_cpuset(cluster, job_id) == cpus
        finally:
            for job_id, _, _ in holders.values():
                cluster.scancel(job_id)
            for job_id, _, _ in holders.values():
                wait_job(cluster, job_id, timeout=120)

        after = _gpu_picks(cluster, share, "pick-after")
        for ordinal, cpus in before.items():
            want = moved_to if ordinal == first else cpus
            assert after.get(ordinal) == want, (
                f"GPU {ordinal}: reserved CPUs {sorted(after.get(ordinal, ()))}, expected {sorted(want)}"
            )
