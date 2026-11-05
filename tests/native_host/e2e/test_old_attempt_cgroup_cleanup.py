# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Real supervisors and kernel cgroups, with controller recovery unavailable.

Each case uses a private Docker cgroup namespace on the first configured node.
"""

import json
import os
from pathlib import Path
import shlex
import uuid

import pytest


@pytest.fixture(scope="module")
def attempt_cleanup_image(ssh_nodes, remote_bin_dir):
    node = ssh_nodes[0]
    code, output = node.exec_with_exit("docker info --format '{{.CgroupVersion}}'")
    if code or output.strip() != "2":
        pytest.skip("attempt cleanup tests require Docker access with cgroup v2")
    token = uuid.uuid4().hex
    directory = f"/tmp/spur-attempt-cleanup-{token}"
    image = f"spur-attempt-cleanup:{token}"
    support = Path(__file__).with_name("attempt_cleanup")
    try:
        node.exec(f"mkdir -p {directory}/bin")
        for name in ("spurd", "spurstepd"):
            node.exec(f"cp {shlex.quote(remote_bin_dir + '/' + name)} {directory}/bin/{name}")
        for name in ("Dockerfile", "probe.py"):
            node.upload(str(support / name), f"{directory}/{name}")
        node.upload(str(Path(__file__).resolve().parents[3] / "proto/slurm.proto"),
                    f"{directory}/slurm.proto")
        base = shlex.quote(os.environ.get("SPUR_TEST_CGROUP_BASE_IMAGE", "ubuntu:22.04"))
        code, _ = node.exec_with_exit(
            f"docker build --build-arg BASE_IMAGE={base} -t {image} {directory} "
            f"> {directory}/build.log 2>&1"
        )
        assert code == 0, node.read_file(f"{directory}/build.log")
        yield node, image, token, directory
    finally:
        node.exec_allow_fail(f"docker image rm {image}")
        node.exec_allow_fail(f"rm -rf {directory}")


@pytest.mark.parametrize("mode", ["batch", "allocation"])
@pytest.mark.parametrize("scenario", [
    "distinct-slots", "shared-slots", "orphaned", "nested-orphans",
    "parent-busy", "leaf-busy",
])
def test_old_attempt_processes_and_cgroups_are_reaped(
    attempt_cleanup_image, tmp_path, scenario, mode,
):
    node, image, token, directory = attempt_cleanup_image
    name = f"spur-attempt-cleanup-{token}-{scenario}-{mode}"
    try:
        code, output = node.exec_with_exit(
            f"docker run --name {name} --init --user 0:0 --privileged --cgroupns=private "
            f"--network=none --hostname=attempt-node {image} "
            f"--scenario {scenario} --mode {mode}"
        )
        (tmp_path / "console.log").write_text(output)
        evidence = f"{directory}/{scenario}-{mode}"
        node.exec(f"docker cp {name}:/evidence {evidence}")
        for filename in ("result.json", "spurd.log"):
            (tmp_path / filename).write_text(node.read_file(f"{evidence}/{filename}"))
        assert code == 0, output
        result = json.loads((tmp_path / "result.json").read_text())
        (tmp_path / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        assert result["probe_completed"] and result["cleanup_before_acceptance"], result
    finally:
        node.exec_allow_fail(f"docker rm -f -v {name}")
        code, _ = node.exec_with_exit(f"docker inspect {name}")
        assert code != 0, f"test container {name} survived cleanup"
