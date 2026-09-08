# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E for --container-remap-root (rootless-root via idmapped mounts, spur#778).

A root-layout image (everything owned by root, including /root) is run both
ways under a root spurd. Without the flag the job is the submitting user and
cannot write root-owned image paths; with the flag it is uid 0 inside a user
namespace and writes to /root succeed — while never becoming host root.
"""

import time

from cluster import job_state, parse_job_id

# Pure bash (minimal image). Report the in-container uid and whether a write to
# the root-owned /root succeeds. The trailing sleep keeps the container running
# past the controller's post-launch dispatch confirmation — a sub-second
# container job otherwise exits before it is confirmed and gets requeued.
PROBE = """#!/bin/bash
echo "UID=$(id -u)"
if echo hi > /root/remap-test 2>/dev/null; then echo "ROOT_WRITE=ok"; else echo "ROOT_WRITE=fail"; fi
echo REMAP_PROBE_OK
sleep 5
"""

# The image ships a root-owned /root (mkdir; -all-root packs it owned by root).
ROOTFS_EXTRA = 'mkdir -p "$R/root"; chmod 700 "$R/root"'


def _warmup(cluster) -> None:
    # The first job after a fresh deploy races the controller's post-launch
    # dispatch confirmation and gets requeued; a throwaway job absorbs that cold
    # start so the job under test is not the first one.
    script = cluster.write_file("warmup.sh", "#!/bin/bash\nsleep 3\necho WARM\n")
    jid = parse_job_id(cluster.sbatch(["-J", "warmup", "-N", "1", script]))
    deadline = time.time() + 40
    while time.time() < deadline:
        if job_state(cluster.squeue_all(), jid) in ("R", "CD", "F", None):
            return
        time.sleep(2)


def _run(cluster, tmp_path, remap: bool) -> tuple[dict, str]:
    cluster.container_preflight()
    # Remap needs a root daemon (chown + parent-written id maps).
    cluster.start(agent_as_root=True)
    _warmup(cluster)
    img = cluster.build_container_image(tmp_path, rootfs_extra=ROOTFS_EXTRA, all_root=True)
    probe = cluster.write_file("remap-probe.sh", PROBE)
    out = f"{cluster.remote_dir}/remap-{remap}.out"
    args = ["-J", "remap", "-N", "1", f"--container-image={img}", "-o", out]
    if remap:
        args.append("--container-remap-root")
    args.append(probe)
    sb = cluster.sbatch(args)
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    content = cluster.wait_output(out, "REMAP_PROBE_OK", timeout=120)
    vals: dict[str, str] = {}
    for line in content.splitlines():
        key, sep, value = line.partition("=")
        if sep:
            vals[key.strip()] = value.strip()
    return vals, content


class TestContainerRemapRoot:
    def test_without_remap_runs_as_submitter(self, unstarted_cluster, tmp_path):
        # Baseline: the container runs as the submitting user and cannot write a
        # root-owned image path — the failure --container-remap-root fixes.
        vals, content = _run(unstarted_cluster, tmp_path, remap=False)
        assert vals.get("UID") not in (None, "0"), (
            f"without the flag the job should run as the submitter, not root:\n{content}"
        )
        assert vals.get("ROOT_WRITE") == "fail", (
            f"the submitter must NOT be able to write root-owned /root:\n{content}"
        )

    def test_remap_runs_as_root_and_writes_root_paths(self, unstarted_cluster, tmp_path):
        # With the flag: uid 0 inside, and root-owned /root is writable.
        vals, content = _run(unstarted_cluster, tmp_path, remap=True)
        assert vals.get("UID") == "0", (
            f"--container-remap-root should make `id -u` == 0 inside:\n{content}"
        )
        assert vals.get("ROOT_WRITE") == "ok", (
            f"the mapped container-root must be able to write root-owned /root:\n{content}"
        )
