# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for RDMA device isolation on a node that hands out NICs per job.

Node 0 needs three ``/dev/infiniband/uverbs*`` nodes the test user can open. A
node with real ones uses them. Otherwise the fixture recreates real character
devices on node 0 as stand-ins: ones from ``STAND_IN_CANDIDATES`` that are
already world read-write and that the test user can open, under
``/dev/infiniband`` with their own mode, so they grant nothing new. It needs
three distinct device numbers, plus a fourth for the GPU step test, which is
skipped without it, and it removes exactly what it created. The first two are
declared as ``nic`` GRES; the third stands for an HCA Spur does not manage,
which must stay open to every job.
"""

import re
from dataclasses import dataclass

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

# A job denied a stand-in is denied its source too (the device filter keys on
# the device number), so none of these may be one a job shell needs.
STAND_IN_CANDIDATES = ["/dev/full", "/dev/net/tun", "/dev/fuse", "/dev/vsock"]
NEVER_STAND_IN = {"/dev/null", "/dev/zero", "/dev/random", "/dev/urandom", "/dev/tty", "/dev/ptmx"}


@dataclass(frozen=True)
class CharDevice:
    path: str
    mode: int
    major: int
    minor: int


def _parse_stat_line(line: str) -> CharDevice | None:
    """Parse ``<path> <%F> <%a> <%t> <%T>``; None unless a world read-write char device."""
    tokens = line.strip().split()
    if len(tokens) < 5:
        return None
    path, file_type = tokens[0], " ".join(tokens[1:-3])
    try:
        mode, major, minor = int(tokens[-3], 8), int(tokens[-2], 16), int(tokens[-1], 16)
    except ValueError:
        return None
    if file_type != "character special file" or mode & 0o006 != 0o006:
        return None
    if path in NEVER_STAND_IN or path.startswith("/dev/pts/"):
        return None
    return CharDevice(path, mode, major, minor)


def _stand_in_sources(node) -> list[CharDevice]:
    """Candidates the test user can already open, one per device number."""
    listed = node.exec_allow_fail(
        f"for p in {' '.join(STAND_IN_CANDIDATES)}; do "
        "[ -c \"$p\" ] && : < \"$p\" 2>/dev/null "
        "&& echo \"$p $(stat -c '%F %a %t %T' \"$p\")\"; done"
    )
    sources, seen = [], set()
    for device in filter(None, map(_parse_stat_line, listed.splitlines())):
        if (device.major, device.minor) not in seen:
            seen.add((device.major, device.minor))
            sources.append(device)
    return sources


def _openable_uverbs(cluster) -> list[str]:
    listed = cluster.nodes[0].exec_allow_fail(
        "for p in /dev/infiniband/uverbs*; do [ -c \"$p\" ] && : < \"$p\" 2>/dev/null "
        "&& echo \"$p\"; done"
    )
    return sorted(listed.split(), key=lambda p: int(re.sub(r"\D", "", p) or 0))


def _device_number(node, path: str) -> tuple[int, int]:
    major, minor = node.exec(f"stat -c '%t %T' {path}").split()
    return int(major, 16), int(minor, 16)


@pytest.fixture
def standin_infiniband(unstarted_cluster):
    """Three uverbs node paths on node 0 and their device numbers: real ones, or
    stand-ins created here."""
    cluster = unstarted_cluster
    node = cluster.nodes[0]
    if node.exec_allow_fail("test -e /dev/infiniband && echo yes").strip() == "yes":
        real = _openable_uverbs(cluster)
        if len(real) < 3:
            pytest.skip(
                f"node 0 has /dev/infiniband but {len(real)} openable uverbs nodes; "
                "refusing to add stand-ins next to real devices"
            )
        yield real[:3], {_device_number(node, p) for p in real[:3]}
        return

    sources = _stand_in_sources(node)
    if len(sources) < 3:
        pytest.skip(
            f"node 0 has {len(sources)} distinct world read-write char devices to stand "
            "in for uverbs; need 3"
        )
    sudo = cluster._sudo_prefix()
    if "ok" not in node.exec_allow_fail(f"{sudo}true && echo ok"):
        pytest.skip("creating stand-in RDMA nodes needs sudo on node 0")
    created_dir, created = False, []
    try:
        node.exec(f"{sudo}mkdir /dev/infiniband")
        created_dir = True
        for i, source in enumerate(sources[:3]):
            path = f"/dev/infiniband/uverbs{i}"
            node.exec(f"{sudo}mknod -m {source.mode:o} {path} c {source.major} {source.minor}")
            created.append(path)
        yield created, {(s.major, s.minor) for s in sources[:3]}
    finally:
        if created:
            node.exec_allow_fail(f"{sudo}rm -f {' '.join(created)}")
        if created_dir:
            node.exec_allow_fail(f"{sudo}rmdir /dev/infiniband")


@pytest.fixture
def gpu_stand_in(unstarted_cluster, standin_infiniband):
    """A device node for a GRES GPU that shares no device number with the NICs."""
    _, used = standin_infiniband
    for source in _stand_in_sources(unstarted_cluster.nodes[0]):
        if (source.major, source.minor) not in used:
            return source.path
    pytest.skip("no fourth distinct world read-write char device for the GPU stand-in")


def _probe_body(paths: list[str]) -> str:
    return "".join(
        f'if err=$( {{ : < {p}; }} 2>&1 ); then echo "{p}=OPEN"; '
        f'else case "$err" in *"Operation not permitted"*) echo "{p}=EPERM";; '
        f'*) echo "{p}=OTHER";; esac; fi\n'
        for p in paths
    )


def _probe_script(paths: list[str]) -> str:
    return "#!/bin/bash\n" + _probe_body(paths) + "echo PROBE_DONE\n"


def _parse(out: str) -> dict:
    return dict(re.findall(r"^(\S+?)=(\S+)\r?$", out, re.MULTILINE))


def _start(cluster, nics: list[str], constrain_rdma: bool, gpu: str | None = None) -> None:
    gres = [{"name": "nic", "type": "standin", "file": p} for p in nics]
    if gpu:
        gres.append({"name": "gpu", "type": "standin", "file": gpu})
    cluster.root_agent_preflight()
    cluster.start(
        {
            "devices": {"auto_detect": False, "gres": gres},
            "cgroup": {"constrain_rdma_devices": constrain_rdma},
        },
        agent_as_root=True,
    )
    assert cluster.spurd_agent_user(0) == "root", "the device filter needs a rootful agent"


def _submit(cluster, name: str, script: str, gres: str | None) -> int:
    args = ["-J", name, "-N", "1", "-w", cluster.node_names[0],
            "-o", f"{cluster.remote_dir}/{name}.out"]
    if gres:
        args.append(f"--gres={gres}")
    job_id = parse_job_id(cluster.sbatch(args + [script]))
    assert job_id is not None, f"sbatch {name} failed"
    return job_id


def _probe(cluster, name: str, paths: list[str], gres: str | None) -> dict:
    script = cluster.write_file(f"{name}.sh", _probe_script(paths))
    job_id = _submit(cluster, name, script, gres)
    wait_job(cluster, job_id, timeout=120)
    out = cluster.wait_output(f"{cluster.remote_dir}/{name}.out", "PROBE_DONE")
    assert "PROBE_DONE" in out, f"{cluster.debug_job(job_id)}\noutput:\n{out}"
    return _parse(out)


def _assert_holds_one_nic(seen: dict, nic0: str, nic1: str, unmanaged: str) -> None:
    assert sorted([seen[nic0], seen[nic1]]) == ["EPERM", "OPEN"], seen
    assert seen[unmanaged] == "OPEN", f"an HCA Spur does not manage must stay open: {seen}"


class TestRdmaIsolation:
    def test_only_allocated_nics_are_reachable(self, unstarted_cluster, standin_infiniband):
        cluster = unstarted_cluster
        (nic0, nic1, unmanaged), _ = standin_infiniband
        _start(cluster, [nic0, nic1], constrain_rdma=True)
        paths = [nic0, nic1, unmanaged]

        none = _probe(cluster, "rdma-none", paths, None)
        assert none[nic0] == "EPERM" and none[nic1] == "EPERM", none
        assert none[unmanaged] == "OPEN", f"an HCA Spur does not manage must stay open: {none}"

        hold = cluster.write_file("rdma-hold.sh", "#!/bin/bash\nsleep 300\n")
        holder = _submit(cluster, "rdma-hold", hold, "nic:1")
        try:
            wait_job_state(cluster, holder, "R", timeout=120)
            _assert_holds_one_nic(_probe(cluster, "rdma-one", paths, "nic:1"), nic0, nic1, unmanaged)
        finally:
            cluster.scancel(holder)

    def test_opting_out_keeps_rdma_open(self, unstarted_cluster, standin_infiniband):
        cluster = unstarted_cluster
        (nic0, nic1, unmanaged), _ = standin_infiniband
        _start(cluster, [nic0, nic1], constrain_rdma=False)

        none = _probe(cluster, "rdma-open", [nic0, nic1, unmanaged], None)
        assert set(none.values()) == {"OPEN"}, none

    def test_a_gpu_step_inside_a_nic_job_keeps_the_jobs_rdma_gating(
        self, unstarted_cluster, standin_infiniband, gpu_stand_in
    ):
        cluster = unstarted_cluster
        (nic0, nic1, unmanaged), _ = standin_infiniband
        _start(cluster, [nic0, nic1], constrain_rdma=True, gpu=gpu_stand_in)
        probe = cluster.write_file("rdma-step-probe.sh", _probe_script([nic0, nic1, unmanaged]))
        batch = cluster.write_file(
            "rdma-step.sh",
            f"#!/bin/bash\nsrun -N 1 -n 1 --gpus=1 bash {probe}\necho BATCH_DONE\n",
        )
        job_id = _submit(cluster, "rdma-step", batch, "gpu:1,nic:1")
        wait_job(cluster, job_id, timeout=180)
        out = cluster.wait_output(f"{cluster.remote_dir}/rdma-step.out", "BATCH_DONE")
        assert "PROBE_DONE" in out, f"{cluster.debug_job(job_id)}\noutput:\n{out}"
        _assert_holds_one_nic(_parse(out), nic0, nic1, unmanaged)

    def test_an_interactive_allocation_step_is_gated(self, unstarted_cluster, standin_infiniband):
        cluster = unstarted_cluster
        (nic0, nic1, unmanaged), _ = standin_infiniband
        _start(cluster, [nic0, nic1], constrain_rdma=True)

        code, out = cluster.salloc_run(
            _probe_body([nic0, nic1, unmanaged]) + "echo PROBE_DONE\n",
            salloc_args=["-N", "1", "-w", cluster.node_names[0], "--gres=nic:1",
                         "--interactive-step"],
            all_nodes=True,
        )
        assert code == 0 and "PROBE_DONE" in out, out
        _assert_holds_one_nic(_parse(out), nic0, nic1, unmanaged)

    def test_a_job_adopted_after_a_spurd_restart_keeps_its_gating(
        self, unstarted_cluster, standin_infiniband
    ):
        cluster = unstarted_cluster
        (nic0, nic1, unmanaged), _ = standin_infiniband
        _start(cluster, [nic0, nic1], constrain_rdma=True)
        go = f"{cluster.remote_dir}/rdma-adopt-go"
        probe = cluster.write_file("rdma-adopt-probe.sh", _probe_script([nic0, nic1, unmanaged]))
        batch = cluster.write_file(
            "rdma-adopt.sh",
            f"#!/bin/bash\nwhile [ ! -e {go} ]; do sleep 1; done\nbash {probe}\n",
        )
        job_id = _submit(cluster, "rdma-adopt", batch, "nic:1")
        try:
            wait_job_state(cluster, job_id, "R", timeout=120)
            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)

            code, step_out = cluster.srun_in_allocation(job_id, ["-N", "1", "-n", "1", "bash", probe])
            assert code == 0 and "PROBE_DONE" in step_out, step_out
            _assert_holds_one_nic(_parse(step_out), nic0, nic1, unmanaged)

            cluster.nodes[0].exec(f"touch {go}")
            wait_job(cluster, job_id, timeout=120)
            out = cluster.wait_output(f"{cluster.remote_dir}/rdma-adopt.out", "PROBE_DONE")
            assert "PROBE_DONE" in out, f"{cluster.debug_job(job_id)}\noutput:\n{out}"
            _assert_holds_one_nic(_parse(out), nic0, nic1, unmanaged)
        finally:
            cluster.scancel(job_id)
