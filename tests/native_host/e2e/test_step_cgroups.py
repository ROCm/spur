# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for Slurm-consistent step cgroup names and contained task hooks.

The nested ``job_<id>_<attempt>/step_<id>`` topology already exists; these assert
the parity behavior this change adds:

* reserved-step leaves are named the Slurm way (``step_batch`` etc.), and a
  numbered ``srun`` step lands in ``step_<n>``;
* ``task_prolog``/``task_epilog`` run as the job user, inside the step cgroup,
  honoring the TaskProlog ``export``/``print`` stdout protocol.

Each probe reads its own ``/proc/self/cgroup`` (no privilege needed); only the
agent must be root, which the ``cgroup_cluster`` fixture enforces.
"""

import os
import re

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

# Pure bash: the minimal container image (reused by the container test) has no
# awk/cut. Emits the process's own cgroup path from the unified (0::) line.
_CG_PROBE = r"""#!/bin/bash
while IFS= read -r line; do
  case "$line" in 0::*) echo "CGROUP=${line#0::}" ;; esac
done < /proc/self/cgroup
echo PROBE_DONE
"""


def _sudo() -> str:
    """Sudo prefix mirroring SpurCluster._sudo_prefix (password via -S, else -n)."""
    pw = os.environ.get("SPUR_TEST_SSH_PASSWORD", "")
    if pw:
        escaped = pw.replace("'", "'\"'\"'")
        return f"echo '{escaped}' | sudo -S "
    return "sudo -n "


def _batch_cgroup(cluster, name: str, extra_args: list[str] | None = None):
    """Run the cgroup probe as a batch job pinned to node 0; return (job_id, cgroup, output)."""
    script = cluster.write_file(f"{name}.sh", _CG_PROBE)
    out_path = f"{cluster.remote_dir}/{name}.out"
    sb = cluster.sbatch(
        ["-J", name, "-N", "1", "-w", cluster.node_names[0], "-o", out_path]
        + (extra_args or [])
        + [script]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    wait_job(cluster, job_id, timeout=120)
    content = cluster.wait_output(out_path, "PROBE_DONE", timeout=120)
    cgroup = None
    for line in content.splitlines():
        if line.startswith("CGROUP="):
            cgroup = line.removeprefix("CGROUP=").strip()
    assert cgroup is not None, (
        f"probe did not report a cgroup\n{content}\n{cluster.debug_job(job_id)}"
    )
    return job_id, cgroup, content


class TestStepCgroupNames:
    """The reserved-step leaves follow Slurm's naming, not the raw StepId ints."""

    def test_batch_lands_in_step_batch(self, cgroup_cluster):
        job_id, cgroup, content = _batch_cgroup(cgroup_cluster, "sn-batch")
        assert cgroup == f"/spur/job_{job_id}_1/step_batch", (
            f"a batch payload must run in the Slurm-named step_batch leaf "
            f"(not step_<numeric-id>), got {cgroup!r}\n{content}"
        )

    def test_container_batch_lands_in_step_batch(self, cgroup_cluster, tmp_path):
        cluster = cgroup_cluster
        cluster.container_preflight()
        image = cluster.build_container_image(tmp_path)
        job_id, cgroup, content = _batch_cgroup(
            cluster, "sn-cbatch", ["--mem=256", f"--container-image={image}"]
        )
        # The container's /proc reports the host cgroup path (no cgroup-ns remap).
        assert cgroup == f"/spur/job_{job_id}_1/step_batch", (
            f"a containerized batch payload must run in step_batch, got {cgroup!r}\n{content}"
        )

    def test_srun_step_lands_in_a_numbered_leaf(self, cgroup_cluster):
        cluster = cgroup_cluster
        script = cluster.write_file("sn-hold.sh", "#!/bin/bash\nsleep 300\n")
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "sn-hold", "-N", "1", "-w", cluster.node_names[0], "-t", "5",
                 "--cpus-per-task=1", "--mem=256", script]
            )
        )
        assert job_id is not None, "sbatch failed to return a job id"
        wait_job_state(cluster, job_id, "R", timeout=90)
        try:
            code, out = cluster.srun_in_allocation(job_id, ["cat", "/proc/self/cgroup"])
        finally:
            cluster.scancel(str(job_id))
        assert re.search(rf"/spur/job_{job_id}_1/step_(\d+)\b", out), (
            f"an srun step must run in a numbered step_<n> leaf under the job "
            f"envelope (exit {code})\noutput:\n{out}"
        )


# Task hooks are node hooks read at agent start; the hardened validator refuses a
# hook that is not an absolute path owned by root and not group/world-writable, so
# the scripts are placed at a fixed path and chowned to root before the cluster starts.
_HOOK_DIR = "/tmp/spur-e2e-taskhook"
_PROLOG = f"{_HOOK_DIR}/task_prolog.sh"
_EPILOG = f"{_HOOK_DIR}/task_epilog.sh"
_PROLOG_MARKER = f"{_HOOK_DIR}/prolog.marker"
_EPILOG_MARKER = f"{_HOOK_DIR}/epilog.marker"

# stdout carries the TaskProlog protocol (export/print); evidence is redirected
# to a marker file so it is not parsed as a directive.
_PROLOG_BODY = f"""#!/bin/bash
echo "export TASKHOOK_INJECTED=yes"
echo "print HOOK_PRINT_MARKER"
{{ printf 'uid=%s\\n' "$(/usr/bin/id -u)"; printf 'cgroup=%s\\n' "$(< /proc/self/cgroup)"; }} > "{_PROLOG_MARKER}" 2>&1
"""

_EPILOG_BODY = f"""#!/bin/bash
printf 'uid=%s ran=1\\n' "$(/usr/bin/id -u)" > "{_EPILOG_MARKER}" 2>&1
"""


@pytest.fixture
def _task_hook_files(ssh_nodes):
    sudo = _sudo()
    for node in ssh_nodes:
        node.exec(f"mkdir -p {_HOOK_DIR}")
        node.exec_allow_fail(f"rm -f {_PROLOG_MARKER} {_EPILOG_MARKER}")
        node.write_file(_PROLOG, _PROLOG_BODY, mode=0o755)
        node.write_file(_EPILOG, _EPILOG_BODY, mode=0o755)
        # A root agent requires the hook be root-owned; write_file created them as
        # the ssh user, so hand ownership to root (dir stays user-owned so the
        # job-user hook can still write its marker there).
        node.exec(f"{sudo}chown root:root {_PROLOG} {_EPILOG}")
        node.exec(f"{sudo}chmod 755 {_PROLOG} {_EPILOG}")
    yield
    for node in ssh_nodes:
        node.exec_allow_fail(f"{sudo}rm -rf {_HOOK_DIR}")


class TestTaskHookContainment:
    """task_prolog/task_epilog run as the job user, inside the step cgroup, with
    the TaskProlog export/print protocol — closing design-doc gap G16."""

    @pytest.fixture
    def cluster_config_overrides(self, _task_hook_files):
        return {"hooks": {"task_prolog": _PROLOG, "task_epilog": _EPILOG}}

    def test_task_hooks_run_contained_with_the_taskprolog_protocol(self, cgroup_cluster):
        cluster = cgroup_cluster
        job_uid = cluster.nodes[0].exec("id -u").strip()

        payload = cluster.write_file(
            "th-payload.sh",
            "#!/bin/bash\n"
            'echo "PAYLOAD TASKHOOK_INJECTED=$TASKHOOK_INJECTED"\n',
        )
        out_path = f"{cluster.remote_dir}/th.out"
        sb = cluster.sbatch(
            ["-J", "th", "-N", "1", "-w", cluster.node_names[0], "-t", "2",
             "--cpus-per-task=1", "--mem=256", "-o", out_path, payload]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        wait_job(cluster, job_id, timeout=120)
        out = cluster.wait_output(out_path, "PAYLOAD", timeout=120)

        # TaskProlog `print` output precedes the payload's own stdout.
        assert "HOOK_PRINT_MARKER" in out and "PAYLOAD" in out, (
            f"missing TaskProlog print or payload output\n{out}\n{cluster.debug_job(job_id)}"
        )
        assert out.index("HOOK_PRINT_MARKER") < out.index("PAYLOAD"), (
            f"TaskProlog `print` output must precede the payload\n{out}"
        )
        # TaskProlog `export` reached the payload environment.
        assert "TASKHOOK_INJECTED=yes" in out, (
            f"TaskProlog `export` must reach the payload env\n{out}"
        )

        # TaskProlog ran as the job user (privilege dropped from the root agent)...
        prolog_marker = cluster.nodes[0].read_file(_PROLOG_MARKER)
        m = re.search(r"uid=(\d+)", prolog_marker)
        assert m and m.group(1) == job_uid and m.group(1) != "0", (
            f"TaskProlog must run as the job user ({job_uid}), not root\n"
            f"marker:\n{prolog_marker!r}"
        )
        # ...inside the step's cgroup leaf, not the agent's cgroup.
        assert f"/spur/job_{job_id}_1/step_batch" in prolog_marker, (
            f"TaskProlog must run inside the step cgroup\nmarker:\n{prolog_marker!r}"
        )

        # TaskEpilog ran too (as the job user).
        epilog_marker = cluster.nodes[0].read_file(_EPILOG_MARKER)
        assert "ran=1" in epilog_marker, (
            f"TaskEpilog must run\nmarker:\n{epilog_marker!r}"
        )
        em = re.search(r"uid=(\d+)", epilog_marker)
        assert em and em.group(1) == job_uid, (
            f"TaskEpilog must run as the job user ({job_uid})\nmarker:\n{epilog_marker!r}"
        )
