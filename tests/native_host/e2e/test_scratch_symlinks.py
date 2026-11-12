# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""spurd, running as root, never follows a link a job plants where it writes.

A job runs as an unprivileged user and picks its own work dir. Each test plants
a link at a name a root spurd used to write through, then checks that a
root-owned canary on the node is unchanged. Only the agent must be root, which
the ``cgroup_cluster`` fixture enforces.
"""

import os
import time

import pytest


def _sudo() -> str:
    """Sudo prefix mirroring SpurCluster._sudo_prefix (password via -S, else -n)."""
    pw = os.environ.get("SPUR_TEST_SSH_PASSWORD", "")
    if pw:
        escaped = pw.replace("'", "'\"'\"'")
        return f"echo '{escaped}' | sudo -S "
    return "sudo -n "


@pytest.fixture
def canary(cgroup_cluster):
    """A root-owned folder holding a root-owned file on node 0, outside anything a job may write."""
    node = cgroup_cluster.nodes[0]
    base = f"/var/tmp/spur-e2e-canary-{time.time_ns()}"
    node.exec(
        f"{_sudo()}mkdir -m 755 {base} && "
        f"echo ORIGINAL | {_sudo()}tee {base}/file >/dev/null && "
        f"{_sudo()}chmod 644 {base}/file"
    )
    yield base
    node.exec_allow_fail(f"{_sudo()}rm -rf {base}")


def _ownership(node, path: str) -> str:
    return node.exec(f"{_sudo()}stat -c '%U:%G %a' {path}").strip()


def _allocation_on_node0(cluster) -> list[str]:
    return ["-N", "1", "-w", cluster.node_names[0], "-t", "0:05"]


class TestRootScratchWritesIgnorePlantedLinks:
    def test_a_step_folder_planted_in_the_work_dir_is_left_alone(
        self, cgroup_cluster, canary
    ):
        cluster = cgroup_cluster
        node = cluster.nodes[0]
        work = f"{cluster.remote_dir}/planted-step-dir-{time.time_ns()}"
        body = (
            f"mkdir -p {work}\n"
            f'ln -s {canary} "{work}/.spur_step_$SPUR_JOB_ID"\n'
            f"srun --label --chdir {work} true\n"
        )

        code, out = cluster.salloc_run(body, salloc_args=_allocation_on_node0(cluster))

        assert code == 0, out
        assert _ownership(node, canary) == "root:root 755", (
            "root must never create, chmod or chown a step folder in the job's work dir"
        )
        listing = node.exec(f"{_sudo()}ls -A {canary}").split()
        assert listing == ["file"], f"no step script may land in the planted folder: {listing}"

    def test_a_job_cannot_plant_links_in_its_spool_folder(self, cgroup_cluster, canary):
        cluster = cgroup_cluster
        node = cluster.nodes[0]
        body = (
            "srun --label true\n"
            'spool="/var/spool/spur/job$SPUR_JOB_ID"\n'
            f'if ln -s {canary}/file "$spool/spur_step9.sh" 2>/dev/null; '
            "then echo PLANTED; else echo REFUSED; fi\n"
            "srun --label true\n"
        )

        code, out = cluster.salloc_run(body, salloc_args=_allocation_on_node0(cluster))

        assert code == 0, out
        assert "REFUSED" in out and "PLANTED" not in out, (
            f"the job's spool folder must not accept entries from the job:\n{out}"
        )
        assert _ownership(node, f"{canary}/file") == "root:root 644"
        assert node.exec(f"{_sudo()}cat {canary}/file").strip() == "ORIGINAL"
