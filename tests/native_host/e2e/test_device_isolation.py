# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for kernel-enforced device isolation (cgroup-v2 BPF device filter).

spurd attaches a default-deny ``BPF_PROG_TYPE_CGROUP_DEVICE`` program to each
job's cgroup, so a job may open only the device nodes its allocation covers.
Unlike the ``*_VISIBLE_DEVICES`` deny in ``test_gpu_deny.py``, which a job can
undo by re-exporting the variable, this one is enforced by the kernel.

These assert on ``/dev/kfd`` rather than on ``/dev/dri/renderD*``. It is the
right probe for two reasons: it reaches a job's allow-list only through an
actual GPU allocation (the injection plan adds it as a shared edit), and it
lives outside ``/dev/dri``, which the namespace wrapper replaces with a tmpfs.
A denial on a render node could therefore be the tmpfs hiding it, while a
denial on ``/dev/kfd`` can only be the filter.

The filter is installed by a rootful agent, so every test here is
``@pytest.mark.rootful``; ``gpu_cluster`` skips the file entirely on nodes
without GPU device nodes.
"""

from typing import NamedTuple

import pytest

from cluster import parse_job_id, wait_job

KFD = "/dev/kfd"

# Opening a device reports one of these. EPERM is the filter denying the open;
# EACCES is ordinary file permissions and would mask the filter, which is why
# _require_unfiltered_access skips rather than letting a test pass vacuously.
_PROBE_HELPER = """\
probe_open() {
  local p="$1" err
  if err=$( { : < "$p"; } 2>&1 ); then
    echo "$p=OPEN"
  else
    case "$err" in
      *"Operation not permitted"*) echo "$p=EPERM" ;;
      *"Permission denied"*)       echo "$p=EACCES" ;;
      *"No such file or directory"*) echo "$p=ENOENT" ;;
      *) echo "$p=OTHER[$err]" ;;
    esac
  fi
}
"""


def _probe_script(body: str) -> str:
    return f"#!/bin/bash\n{_PROBE_HELPER}{body}echo DEVICE_PROBE_OK\n"


class _Probe(NamedTuple):
    output: str
    job_id: int


def _run_probe(cluster, name: str, body: str, sbatch_args: list[str]) -> _Probe:
    """Submit a probe pinned to node 0 and return its output and job id.

    Pinned because the allocation, and therefore the allow-list, is per node;
    an unpinned job could land somewhere the assertion was not written for.
    """
    script = cluster.write_file(f"{name}.sh", _probe_script(body))
    out_path = f"{cluster.remote_dir}/{name}.out"
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-o", out_path]
        + sbatch_args
        + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"

    wait_job(cluster, job_id, timeout=120)
    content = cluster.wait_output(out_path, "DEVICE_PROBE_OK", timeout=120)
    assert "DEVICE_PROBE_OK" in content, (
        f"probe did not run to completion\n{cluster.debug_job(job_id)}\n"
        f"output:\n{content}"
    )
    return _Probe(content, job_id)


def _require_rootful(cluster) -> None:
    user = cluster.spurd_agent_user(0)
    assert user == "root", (
        f"the device filter needs a rootful agent to load and attach the BPF "
        f"program, got user {user!r}"
    )


def _require_unfiltered_access(cluster) -> None:
    """Skip unless the node has /dev/kfd and the test user can already open it.

    If file permissions alone deny the open, every deny assertion below would
    pass for the wrong reason.
    """
    node = cluster.nodes[0]
    if not node.exec_allow_fail(f"ls {KFD} 2>/dev/null").strip():
        pytest.skip(f"node 0 has no {KFD} (not an AMD GPU node)")
    probe = node.exec_allow_fail(
        f'if : < {KFD} 2>/dev/null; then echo OPEN; else echo DENIED; fi'
    ).strip()
    if "OPEN" not in probe:
        pytest.skip(
            f"{KFD} is not openable by the test user outside a job, so file "
            f"permissions would mask the device filter"
        )


@pytest.mark.rootful
class TestDeviceIsolation:
    def test_zero_gpu_job_is_denied_the_gpu_control_node(self, gpu_cluster):
        # The SPUR-192 case: a job that was allocated no GPU shares the node with
        # jobs that were, and must not be able to reach the hardware.
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        _require_rootful(cluster)
        _require_unfiltered_access(cluster)

        content = _run_probe(
            cluster,
            "dev-iso-zero",
            f"probe_open {KFD}\n"
            'head -c 8 /dev/urandom > /dev/null && echo urandom=OPEN\n'
            'echo x > /dev/null && echo null=OPEN\n',
            [],
        ).output

        assert f"{KFD}=EPERM" in content, (
            f"a zero-GPU job must be denied {KFD} by the kernel\noutput:\n{content}"
        )
        # Without these the test would also pass against a filter that denies
        # everything, which would break every job on the node.
        assert "urandom=OPEN" in content, (
            f"base pseudo-devices must stay reachable\noutput:\n{content}"
        )
        assert "null=OPEN" in content, (
            f"base pseudo-devices must stay reachable\noutput:\n{content}"
        )

    def test_allocated_gpu_job_can_open_the_control_node(self, gpu_cluster):
        # The other half of the property: isolation must not cost a job the
        # hardware it was actually given.
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        _require_rootful(cluster)
        _require_unfiltered_access(cluster)

        content = _run_probe(
            cluster, "dev-iso-alloc", f"probe_open {KFD}\n", ["--gres=gpu:1"]
        ).output

        assert f"{KFD}=OPEN" in content, (
            f"a job allocated a GPU must be able to open {KFD}\noutput:\n{content}"
        )

    def test_visible_devices_override_does_not_restore_access(self, gpu_cluster):
        # What Phase 0's env-var deny could not provide: re-exporting the
        # selector is how a job defeats an advisory sentinel, and it must not
        # move a kernel filter.
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        _require_rootful(cluster)
        _require_unfiltered_access(cluster)

        content = _run_probe(
            cluster,
            "dev-iso-reexport",
            "export ROCR_VISIBLE_DEVICES=0\n"
            "export HIP_VISIBLE_DEVICES=0\n"
            "export CUDA_VISIBLE_DEVICES=0\n"
            f"probe_open {KFD}\n",
            [],
        ).output

        assert f"{KFD}=EPERM" in content, (
            f"re-exporting the GPU selectors must not restore access to {KFD}\n"
            f"output:\n{content}"
        )


@pytest.mark.rootful
class TestDeviceIsolationConfig:
    def test_constrain_devices_false_leaves_the_node_reachable(self, gpu_cluster):
        # Proves the deny in the tests above is the filter and not some other
        # layer: the only thing that changes here is the switch.
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        _require_rootful(cluster)
        _require_unfiltered_access(cluster)

        cluster.stop()
        cluster.start({"cgroup": {"constrain_devices": False}}, agent_as_root=True)

        content = _run_probe(
            cluster, "dev-iso-off", f"probe_open {KFD}\n", []
        ).output

        assert f"{KFD}=OPEN" in content, (
            f"with constrain_devices = false a zero-GPU job must still reach "
            f"{KFD}\noutput:\n{content}"
        )

    def test_extra_device_paths_allows_an_unallocated_node(self, gpu_cluster):
        # The operator escape hatch for a device the allocation does not cover
        # and the built-in host-infrastructure list does not name.
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        _require_rootful(cluster)
        _require_unfiltered_access(cluster)

        cluster.stop()
        cluster.start({"cgroup": {"extra_device_paths": [KFD]}}, agent_as_root=True)

        content = _run_probe(
            cluster, "dev-iso-extra", f"probe_open {KFD}\n", []
        ).output

        assert f"{KFD}=OPEN" in content, (
            f"{KFD} listed in extra_device_paths must be allowed even for a "
            f"zero-GPU job\noutput:\n{content}"
        )


@pytest.mark.rootful
class TestDeviceFilterLifecycle:
    def test_filter_is_released_when_the_job_cgroup_goes_away(self, gpu_cluster):
        """Nothing detaches the filter explicitly.

        The program is freed because removing the cgroup directory drops the
        kernel's last reference to it, so a leak here would accumulate one
        program per job for the lifetime of the agent.
        """
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        _require_rootful(cluster)

        node = cluster.nodes[0]
        if not node.exec_allow_fail("command -v bpftool").strip():
            pytest.skip("bpftool is not installed on node 0")
        # Without a working non-interactive sudo the count below would always
        # read 0 and the assertion would hold for the wrong reason.
        if "PROBE_OK" not in node.exec_allow_fail(
            "sudo -n bpftool prog show >/dev/null 2>&1 && echo PROBE_OK"
        ):
            pytest.skip("bpftool needs passwordless sudo on node 0 to list programs")

        def loaded() -> int:
            out = node.exec_allow_fail(
                "sudo -n bpftool prog show 2>/dev/null | grep -c cgroup_device"
            ).strip()
            return int(out) if out.isdigit() else -1

        baseline = loaded()
        assert baseline >= 0, "could not read the loaded cgroup_device program count"

        probe = _run_probe(cluster, "dev-iso-life", "true\n", ["--gres=gpu:1"])
        cgroup = f"/sys/fs/cgroup/spur/job_{probe.job_id}"

        # Scoped to this job's cgroup rather than every job_* directory, so a
        # job belonging to another test cannot decide this one.
        assert not node.exec_allow_fail(f"ls -d {cgroup} 2>/dev/null").strip(), (
            f"{cgroup} outlived its job, so its filter is still attached"
        )
        after = loaded()
        assert after == baseline, (
            f"cgroup_device programs went from {baseline} to {after}; the filter "
            f"is not being released with the job cgroup"
        )
