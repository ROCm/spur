# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for RDMA device isolation on a node that hands out NICs per job.

Needs at least three ``/dev/infiniband/uverbs*`` nodes on node 0 that the test
user can open. The first two are declared as ``nic`` GRES; the third stands for
an HCA Spur does not manage, which must stay open to every job.
"""

import re

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def _uverbs_nodes(cluster) -> list[str]:
    listed = cluster.nodes[0].exec_allow_fail(
        "for p in /dev/infiniband/uverbs*; do [ -c \"$p\" ] && : < \"$p\" 2>/dev/null "
        "&& echo \"$p\"; done"
    )
    nodes = sorted(listed.split(), key=lambda p: int(re.sub(r"\D", "", p) or 0))
    if len(nodes) < 3:
        pytest.skip(f"need 3 openable uverbs nodes on node 0, found {nodes}")
    return nodes[:3]


def _probe_script(paths: list[str]) -> str:
    probes = "".join(
        f'if err=$( {{ : < {p}; }} 2>&1 ); then echo "{p}=OPEN"; '
        f'else case "$err" in *"Operation not permitted"*) echo "{p}=EPERM";; '
        f'*) echo "{p}=OTHER";; esac; fi\n'
        for p in paths
    )
    return "#!/bin/bash\n" + probes + "echo PROBE_DONE\n"


def _start(cluster, nics: list[str], constrain_rdma: bool) -> None:
    cluster.root_agent_preflight()
    cluster.start(
        {
            "devices": {
                "auto_detect": False,
                "gres": [{"name": "nic", "type": "standin", "file": p} for p in nics],
            },
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
    return dict(re.findall(r"^(\S+?)=(\S+)$", out, re.MULTILINE))


class TestRdmaIsolation:
    def test_only_allocated_nics_are_reachable(self, unstarted_cluster):
        cluster = unstarted_cluster
        nic0, nic1, unmanaged = _uverbs_nodes(cluster)
        _start(cluster, [nic0, nic1], constrain_rdma=True)
        paths = [nic0, nic1, unmanaged]

        none = _probe(cluster, "rdma-none", paths, None)
        assert none[nic0] == "EPERM" and none[nic1] == "EPERM", none
        assert none[unmanaged] == "OPEN", f"an HCA Spur does not manage must stay open: {none}"

        hold = cluster.write_file("rdma-hold.sh", "#!/bin/bash\nsleep 300\n")
        holder = _submit(cluster, "rdma-hold", hold, "nic:1")
        try:
            wait_job_state(cluster, holder, "R", timeout=120)
            mine = _probe(cluster, "rdma-one", paths, "nic:1")
            assert sorted([mine[nic0], mine[nic1]]) == ["EPERM", "OPEN"], mine
            assert mine[unmanaged] == "OPEN", mine
        finally:
            cluster.scancel(holder)

    def test_opting_out_keeps_rdma_open(self, unstarted_cluster):
        cluster = unstarted_cluster
        nic0, nic1, unmanaged = _uverbs_nodes(cluster)
        _start(cluster, [nic0, nic1], constrain_rdma=False)

        none = _probe(cluster, "rdma-open", [nic0, nic1, unmanaged], None)
        assert set(none.values()) == {"OPEN"}, none
