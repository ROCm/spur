# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for GPU and NIC injection from static CDI specs.

The specs, written to a ``cdi_spec_dirs`` directory, describe two AMD GPUs and
two AMD AI NICs backed by world-openable stand-in char devices, so the test
runs on nodes without that hardware. It checks the node's GRES, each job's
runtime environment for both kinds, and that the cgroup device filter lets a
job and its steps open only the devices allocated to it.
"""

import json
import re
import shlex

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

GPUS = [  # (stand-in node, PCI address)
    ("/dev/vfio/vfio", "0000:11:00.0"),
    ("/dev/rfkill", "0000:12:00.0"),
]
NICS = [  # (stand-in node, RDMA device, PCI address)
    ("/dev/net/tun", "ionic_0", "0000:05:00.0"),
    ("/dev/vsock", "ionic_1", "0000:85:00.0"),
]
ALL_NODES = [path for path, _ in GPUS] + [path for path, _, _ in NICS]
ENV_VARS = [
    "ROCR_VISIBLE_DEVICES",
    "SPUR_JOB_GPUS",
    "SPUR_JOB_NICS",
    "NCCL_IB_HCA",
    "PCIDEVICE_AMD_COM_NIC",
]


def _device(name: str, path: str, annotations: dict) -> dict:
    return {
        "name": name,
        "annotations": annotations,
        "containerEdits": {"deviceNodes": [{"path": path}]},
    }


def _gpu_spec() -> dict:
    return {
        "cdiVersion": "0.6.0",
        "kind": "amd.com/gpu",
        "devices": [
            _device(
                str(i),
                path,
                {"spur.amd.com/gpu-type": "mi300x", "spur.amd.com/pci-bdf": bdf},
            )
            for i, (path, bdf) in enumerate(GPUS)
        ],
    }


def _nic_spec() -> dict:
    return {
        "cdiVersion": "0.6.0",
        "kind": "amd.com/nic",
        "devices": [
            _device(
                str(i),
                path,
                {
                    "spur.amd.com/device-type": "pollara400",
                    "spur.amd.com/rdma-device": rdma,
                    "spur.amd.com/pci-bdf": bdf,
                },
            )
            for i, (path, rdma, bdf) in enumerate(NICS)
        ],
    }


_REPORT = """\
report() {
  local prefix="$1" p err
""" + "".join(
    f'  echo "${{prefix}}{var}=${{{var}-unset}}"\n' for var in ENV_VARS
) + """  for p in """ + " ".join(ALL_NODES) + """; do
    if err=$( { : < "$p"; } 2>&1 ); then echo "${prefix}$p=OPEN"
    else
      case "$err" in
        *"Operation not permitted"*) echo "${prefix}$p=EPERM" ;;
        *) echo "${prefix}$p=OTHER" ;;
      esac
    fi
  done
}
"""


def _probe_script(with_step: bool) -> str:
    body = "#!/bin/bash\n" + _REPORT + "report job:\n"
    if with_step:
        body += "srun -N 1 -n 1 bash -c " + shlex.quote(_REPORT + "report step:") + "\n"
    return body + "echo PROBE_DONE\n"


def _start(cluster) -> None:
    node = cluster.nodes[0]
    for path in ALL_NODES:
        if "OPEN" not in node.exec_allow_fail(f": < {path} 2>/dev/null && echo OPEN"):
            pytest.skip(f"{path} is not openable by the test user on node 0")
    cdi_dir = f"{cluster.remote_dir}/cdi"
    for i, spec in enumerate([_gpu_spec(), _nic_spec()]):
        cluster.write_file(f"cdi/fake-{i}.json", json.dumps(spec), all_nodes=True,
                           executable=False)
    cluster.root_agent_preflight()
    cluster.start(
        {"devices": {"auto_detect": True, "cdi_spec_dirs": [cdi_dir]}},
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


def _probe(cluster, name: str, gres: str | None, with_step: bool = False) -> dict:
    script = cluster.write_file(f"{name}.sh", _probe_script(with_step))
    job_id = _submit(cluster, name, script, gres)
    wait_job(cluster, job_id, timeout=180)
    out = cluster.wait_output(f"{cluster.remote_dir}/{name}.out", "PROBE_DONE")
    assert "PROBE_DONE" in out, f"{cluster.debug_job(job_id)}\noutput:\n{out}"
    return dict(re.findall(r"^(\S+?)=(\S*)$", out, re.MULTILINE))


def _assert_holds(seen: dict, prefix: str, gpu: int, nic: int) -> None:
    gpu_path, _ = GPUS[gpu]
    nic_path, rdma, bdf = NICS[nic]
    assert seen[f"{prefix}ROCR_VISIBLE_DEVICES"] == str(gpu), seen
    assert seen[f"{prefix}SPUR_JOB_GPUS"] == str(gpu), seen
    assert seen[f"{prefix}SPUR_JOB_NICS"] == str(nic), seen
    assert seen[f"{prefix}NCCL_IB_HCA"] == f"={rdma}", seen
    assert seen[f"{prefix}PCIDEVICE_AMD_COM_NIC"] == bdf, seen
    for path in ALL_NODES:
        expected = "OPEN" if path in (gpu_path, nic_path) else "EPERM"
        assert seen[f"{prefix}{path}"] == expected, f"{prefix}{path}: {seen}"


class TestFakeCdiInjection:
    def test_gpus_and_nics_from_static_cdi_specs(self, unstarted_cluster):
        cluster = unstarted_cluster
        _start(cluster)

        show = cluster.scontrol_show_node(cluster.node_names[0])
        assert "Gres=gpu:mi300x:2,nic:pollara400:2" in show, show

        none = _probe(cluster, "cdi-none", None)
        assert none["job:SPUR_JOB_NICS"] == "unset", none
        assert none["job:NCCL_IB_HCA"] == "unset", none
        assert none["job:PCIDEVICE_AMD_COM_NIC"] == "unset", none
        for path in ALL_NODES:
            assert none[f"job:{path}"] == "EPERM", f"a job without devices reached {path}"

        hold = cluster.write_file("cdi-hold.sh", "#!/bin/bash\nsleep 300\n")
        holder = _submit(cluster, "cdi-hold", hold, "gpu:1,nic:1")
        try:
            wait_job_state(cluster, holder, "R", timeout=120)
            mine = _probe(cluster, "cdi-one", "gpu:1,nic:1", with_step=True)
            gpu = int(mine["job:SPUR_JOB_GPUS"])
            nic = int(mine["job:SPUR_JOB_NICS"])
            _assert_holds(mine, "job:", gpu, nic)
            _assert_holds(mine, "step:", gpu, nic)
        finally:
            cluster.scancel(holder)

        first = _probe(cluster, "cdi-first", "gpu:1,nic:1")
        assert {first["job:SPUR_JOB_GPUS"], first["job:SPUR_JOB_NICS"]} <= {"0", "1"}
        _assert_holds(
            first, "job:", int(first["job:SPUR_JOB_GPUS"]), int(first["job:SPUR_JOB_NICS"])
        )
