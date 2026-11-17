# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for non-GPU devices scheduled per job, using NICs as the kind.

Two world-openable char devices stand in for NICs through ``[[devices.gres]]``,
so the test runs on any node: the node reports them, the scheduler and the
agent hand each to one job, the job gets ``SPUR_JOB_NICS``, and the cgroup
device filter lets it open only its own. Discovery of real Pollara NICs and
RDMA withholding are covered by unit tests, as they need the hardware.
"""

import re
import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

STAND_IN_NICS = ["/dev/net/tun", "/dev/vsock"]

_PROBE = """\
#!/bin/bash
probe_open() {
  local p="$1" err
  if err=$( { : < "$p"; } 2>&1 ); then
    echo "$p=OPEN"
  else
    case "$err" in
      *"Operation not permitted"*) echo "$p=EPERM" ;;
      *) echo "$p=OTHER[$err]" ;;
    esac
  fi
}
echo "SPUR_JOB_NICS=${SPUR_JOB_NICS-unset}"
""" + "".join(f"probe_open {p}\n" for p in STAND_IN_NICS) + "echo PROBE_DONE\n"


def _start_with_stand_in_nics(cluster) -> None:
    node = cluster.nodes[0]
    for path in STAND_IN_NICS:
        if "OPEN" not in node.exec_allow_fail(f": < {path} 2>/dev/null && echo OPEN"):
            pytest.skip(f"{path} is not openable by the test user on node 0")
    cluster.root_agent_preflight()
    cluster.start(
        {
            "devices": {
                "auto_detect": False,
                "gres": [
                    {"name": "nic", "type": "standin", "file": path}
                    for path in STAND_IN_NICS
                ],
            }
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


def _probe(cluster, name: str, gres: str | None) -> dict[str, str]:
    script = cluster.write_file(f"{name}.sh", _PROBE)
    job_id = _submit(cluster, name, script, gres)
    wait_job(cluster, job_id, timeout=120)
    out = cluster.wait_output(f"{cluster.remote_dir}/{name}.out", "PROBE_DONE")
    assert "PROBE_DONE" in out, f"{cluster.debug_job(job_id)}\noutput:\n{out}"
    return dict(re.findall(r"^(\S+?)=(\S+)$", out, re.MULTILINE))


class TestNicDevices:
    def test_nics_are_reported_scheduled_and_isolated_per_job(self, unstarted_cluster):
        cluster = unstarted_cluster
        _start_with_stand_in_nics(cluster)
        node = cluster.node_names[0]

        show = cluster.scontrol_show_node(node)
        assert "nic:standin:2" in show, f"the node must report both NICs\n{show}"

        no_nic = _probe(cluster, "nic-none", None)
        assert no_nic["SPUR_JOB_NICS"] == "unset", no_nic
        for path in STAND_IN_NICS:
            assert no_nic[path] == "EPERM", f"a job without NICs reached {path}: {no_nic}"

        hold = cluster.write_file("nic-hold.sh", "#!/bin/bash\nsleep 300\n")
        holder = _submit(cluster, "nic-hold", hold, "nic:1")
        try:
            wait_job_state(cluster, holder, "R", timeout=120)

            mine = _probe(cluster, "nic-one", "nic:1")
            index = int(mine["SPUR_JOB_NICS"])
            own, other = STAND_IN_NICS[index], STAND_IN_NICS[1 - index]
            assert mine[own] == "OPEN", f"the job must open its own NIC: {mine}"
            assert mine[other] == "EPERM", f"the job reached the held NIC: {mine}"

            both = _submit(cluster, "nic-both", hold, "nic:2")
            try:
                states = set()
                for _ in range(10):
                    states.add(job_state(cluster.squeue_all(), both))
                    time.sleep(2)
                assert states == {"PD"}, (
                    f"two NICs cannot be free while another job holds one: {states}"
                )
            finally:
                cluster.scancel(both)
        finally:
            cluster.scancel(holder)
