# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E for the host-ROCm container library overlay policy.

On a GPU host, spur's auto-detected AMD CDI spec can bind-mount the host's
/opt/rocm/lib over the same path inside every GPU container, replacing the
image's ROCm userspace. That overlay is now gated behind
`[devices] overlay_host_rocm_libs` (default off). These tests plant a provenance
marker in the image's own /opt/rocm/lib and assert which userspace the container
actually sees, and whether the host path is mounted over it.

Requires a real GPU (KFD) and /opt/rocm on the host; skips otherwise.
"""

import pytest

from cluster import parse_job_id, wait_job

MARKER = "spur-provenance"
IMAGE_TAG = "image-userspace"

# Plant the image's own /opt/rocm/lib with a provenance marker before the image
# is packed, so the container can be asked whose libraries it is looking at.
ROOTFS_EXTRA = (
    f'mkdir -p "$R/opt/rocm/lib"; ' f'printf "{IMAGE_TAG}" > "$R/opt/rocm/lib/{MARKER}"'
)

# Pure bash (the minimal image has no grep): report the marker's contents and
# whether a host /opt/rocm/lib mount is overlaid on top of it.
PROBE = f"""#!/bin/bash
echo "PROV=$(cat /opt/rocm/lib/{MARKER} 2>/dev/null || echo MISSING)"
OVERLAY=no
while IFS= read -r line; do
  case "$line" in *"/opt/rocm/lib"*) OVERLAY=yes ;; esac
done < /proc/self/mountinfo
echo "OVERLAY=$OVERLAY"
echo OVERLAY_PROBE_OK
"""


def _node_has_gpu_and_rocm(cluster) -> bool:
    # Each condition must actually gate: a piped `ls | head` always exits 0, so
    # test the device node and a render node directly.
    probe = cluster.nodes[0].exec_allow_fail(
        "test -e /dev/kfd && ls /dev/dri/renderD* >/dev/null 2>&1 "
        "&& test -d /opt/rocm/lib && echo READY"
    )
    return "READY" in probe


def _run_probe(cluster, overlay_on: bool, tmp_path) -> tuple[dict, str]:
    if not _node_has_gpu_and_rocm(cluster):
        pytest.skip("node 0 lacks a GPU (KFD) and/or /opt/rocm/lib")
    cluster.container_preflight()
    # Overlay binds need a root agent; without passwordless sudo start() would
    # otherwise raise instead of skipping cleanly.
    cluster.root_agent_preflight()

    # devices_config() pins auto_detect=True, which the whole feature is gated on.
    cluster.start(
        config_overrides=cluster.devices_config(overlay_host_rocm_libs=overlay_on),
        agent_as_root=True,
    )
    # If the agent isn't actually root the injected binds fail with only a warn,
    # so the overlay-off assertions would pass even with the gate reverted. Skip
    # rather than assert a hollow pass.
    if cluster.spurd_agent_user(0) != "root":
        pytest.skip("overlay binds require a root agent; spurd is not running as root")
    cluster.gpu_preflight(1)

    img = cluster.build_container_image(tmp_path, rootfs_extra=ROOTFS_EXTRA)
    probe = cluster.write_file("overlay-probe.sh", PROBE)
    out = f"{cluster.remote_dir}/overlay-{overlay_on}.out"
    # Pin to node 0: write_file placed the probe on node 0 only, and node 0 is
    # the one _node_has_gpu_and_rocm verified.
    sb = cluster.sbatch(
        ["-J", "rocm-overlay", "-N", "1", "-w", cluster.node_names[0], "--gres=gpu:1",
         f"--container-image={img}", "-o", out, probe]
    )
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    wait_job(cluster, job_id, timeout=180)
    content = cluster.wait_output(out, "OVERLAY_PROBE_OK")

    vals: dict[str, str] = {}
    for line in content.splitlines():
        key, sep, value = line.partition("=")
        if sep:
            vals[key.strip()] = value.strip()
    return vals, content


class TestRocmLibraryOverlay:
    def test_overlay_off_keeps_image_libraries(self, unstarted_cluster, tmp_path):
        # Default: the container keeps its own /opt/rocm/lib, matching
        # `docker run` — no host path is mounted over it.
        vals, content = _run_probe(unstarted_cluster, overlay_on=False, tmp_path=tmp_path)
        assert vals.get("PROV") == IMAGE_TAG, (
            f"the image's /opt/rocm/lib was replaced despite the overlay being off:\n{content}"
        )
        assert vals.get("OVERLAY") == "no", (
            f"host /opt/rocm/lib was overlaid by default:\n{content}"
        )

    def test_overlay_on_replaces_with_host_libraries(self, unstarted_cluster, tmp_path):
        # Opt-in: the host's /opt/rocm/lib is bind-mounted over the image's, so
        # the image marker is hidden and the mount is visible in the mount table.
        vals, content = _run_probe(unstarted_cluster, overlay_on=True, tmp_path=tmp_path)
        assert vals.get("OVERLAY") == "yes", (
            f"host /opt/rocm/lib was not overlaid when opted in:\n{content}"
        )
        assert vals.get("PROV") == "MISSING", (
            f"the image's marker survived a host overlay (host has no marker):\n{content}"
        )
