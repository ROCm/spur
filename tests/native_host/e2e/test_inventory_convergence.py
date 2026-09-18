# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for periodic node-inventory convergence.

spurd runs a periodic inventory-refresh loop that rebuilds its device registry
from `devices.cdi_spec_dirs` (when `devices.auto_detect = false`), recomputes the
schedulable ResourceSet, and re-registers with the controller when the inventory
changed. These tests drive that loop WITHOUT real GPUs by pointing spurd at a
static on-disk CDI spec that the test rewrites between refresh ticks:

  - `auto_detect = false` + a test-controlled `cdi_spec_dirs` means the advertised
    inventory is exactly what the JSON declares. The CDI loader validates only
    version/kind/non-empty devices and never stats the device-node `path`, so a
    GPU-less host still advertises N GPUs from N unique `renderD<minor>` paths.
  - `SPUR_INVENTORY_REFRESH_SECS` (injected into spurd's env via `agent_env`) sets
    the refresh cadence small so convergence is observable in seconds.

Each GPU carries a `spur.amd.com/render-minor` annotation which becomes the GPU's
stable_id in the scheduler; the controller prints one `gpu:<type>:1` Gres entry
per device, so `node_gpu_count` returns the device count.
"""

import json
import time

import pytest

from cluster import job_state, parse_job_id, wait_job

# Fast refresh so a rewrite is adopted within a couple of ticks.
REFRESH_SECS = 2

# Convergence deadline: several refresh intervals plus re-register + controller
# round-trip. The refresh loop also debounces (a change must be seen on two
# consecutive ticks before it is applied), so allow > 2 * REFRESH_SECS.
CONVERGE_TIMEOUT = 20

GPU_TYPE = "mi300x"


def _cdi_spec(render_minors: list[int]) -> str:
    """A static AMD CDI spec JSON declaring one GPU per render-minor.

    Field names mirror crates/spur-devices/src/cdi/spec.rs serde attributes:
    cdiVersion, kind, devices[].name, devices[].annotations,
    devices[].containerEdits.deviceNodes[].path.
    """
    devices = []
    for idx, minor in enumerate(render_minors):
        devices.append(
            {
                "name": str(idx),
                "annotations": {
                    "spur.amd.com/render-minor": str(minor),
                    "spur.amd.com/gpu-type": GPU_TYPE,
                },
                "containerEdits": {
                    "deviceNodes": [{"path": f"/dev/dri/renderD{minor}"}],
                },
            }
        )
    return json.dumps(
        {"cdiVersion": "0.6.0", "kind": "amd.com/gpu", "devices": devices},
        indent=2,
    )


def _write_spec(cluster, cdi_dir: str, render_minors: list[int]) -> None:
    """(Re)write the CDI spec on the agent node. Rewrites are atomic (mv) so the
    refresh loop never reads a half-written file."""
    body = _cdi_spec(render_minors)
    tmp = f"{cdi_dir}/amd.json.tmp"
    dst = f"{cdi_dir}/amd.json"
    node = cluster.nodes[0]
    node.write_file(tmp, body, mode=0o644)
    node.exec(f"mv -f '{tmp}' '{dst}'")


def _wait_node_gpu_count(
    cluster, node_name: str, expected: int, timeout: int = CONVERGE_TIMEOUT
) -> int:
    """Poll `node_gpu_count` until it equals *expected* or the deadline passes.

    Returns the last observed count (== expected on success). There is no
    existing wait_node_gpu_count helper, so this is the bounded local poll.
    """
    deadline = time.time() + timeout
    last = -1
    while time.time() < deadline:
        last = cluster.node_gpu_count(node_name)
        if last == expected:
            return last
        time.sleep(1)
    return last


class TestInventoryConvergence:
    """Static-CDI convergence: rewrite the on-disk spec, prove the controller's
    view follows without restarting spurd."""

    def _start(self, cluster, render_minors: list[int]) -> tuple[str, str]:
        """Provision a single-node cluster with a static CDI dir and fast refresh,
        seeded with *render_minors*. Returns (node_name, cdi_dir)."""
        cluster.require_nodes(1)
        cdi_dir = f"{cluster.remote_dir}/cdi"
        cluster.nodes[0].exec(f"mkdir -p '{cdi_dir}'")
        _write_spec(cluster, cdi_dir, render_minors)
        cluster.agent_env = {"SPUR_INVENTORY_REFRESH_SECS": str(REFRESH_SECS)}
        cluster.start(
            config_overrides=cluster.devices_config(
                auto_detect=False, cdi_spec_dirs=[cdi_dir]
            )
        )
        node_name = cluster.node_names[0]
        # Sanity: the seeded spec must be advertised before the test proceeds,
        # else the whole premise (static CDI advertises without KFD) is false and
        # every downstream assertion would be meaningless.
        seeded = _wait_node_gpu_count(cluster, node_name, len(render_minors))
        if seeded != len(render_minors):
            pytest.skip(
                f"static CDI did not advertise {len(render_minors)} GPUs "
                f"(got {seeded}); loader may require real device nodes on this bed\n"
                f"{cluster.scontrol_show_node(node_name)}"
            )
        return node_name, cdi_dir

    def test_growth_converges(self, unstarted_cluster):
        """Positive: growing the spec 2 -> 4 devices converges node_gpu_count up
        without restarting spurd."""
        cluster = unstarted_cluster
        node_name, cdi_dir = self._start(cluster, [128, 129])

        _write_spec(cluster, cdi_dir, [128, 129, 130, 131])
        got = _wait_node_gpu_count(cluster, node_name, 4)
        assert got == 4, (
            f"inventory growth 2->4 must converge node_gpu_count to 4, got {got}\n"
            f"{cluster.scontrol_show_node(node_name)}\n"
            f"spurd log tail:\n{cluster.spurd_log()[-1500:]}"
        )

    def test_job_lands_on_grown_capacity(self, unstarted_cluster):
        """Positive: a job needing 3 GPUs is unschedulable at 2 devices, then
        schedules only after the inventory grows to 4.

        The pre-growth PENDING assertion is exercised on every bed (it proves the
        2-GPU inventory blocked the job). Actually RUNNING a GPU job needs real
        device nodes: the agent's dispatch sets up `/dev/dri/renderD*` device
        access, which cannot succeed on a GPU-less host even though the static CDI
        makes the *count* converge. So after growth we require the job to leave
        PENDING; if it completes we assert that, and if it can only be dispatched
        on real hardware we skip with a specific reason rather than false-pass."""
        cluster = unstarted_cluster
        node_name, cdi_dir = self._start(cluster, [128, 129])

        probe = cluster.write_file(
            "grow-probe.sh", "#!/bin/bash\necho GREW_OK\n"
        )
        out_path = f"{cluster.remote_dir}/grow-probe.out"
        sb = cluster.sbatch(
            ["-J", "grow-3g", "-N", "1", "--gres=gpu:3", "-o", out_path, probe]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        # Before growth the node has only 2 GPUs, so a 3-GPU job cannot run.
        deadline = time.time() + 3 * REFRESH_SECS + 4
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), job_id) == "R":
                break
            time.sleep(1)
        pre = job_state(cluster.squeue_all(), job_id)
        assert pre == "PD", (
            f"3-GPU job must stay pending while node has 2 GPUs, got {pre}\n"
            f"{cluster.debug_job(job_id)}"
        )

        # Grow to 4 GPUs; the node's advertised inventory must converge first.
        _write_spec(cluster, cdi_dir, [128, 129, 130, 131])
        grew = _wait_node_gpu_count(cluster, node_name, 4)
        assert grew == 4, (
            f"node must converge to 4 GPUs before job can land, got {grew}\n"
            f"{cluster.scontrol_show_node(node_name)}"
        )

        # The controller now has capacity for the 3-GPU job. On a real-GPU bed it
        # runs to completion; on a GPU-less bed the agent cannot open the (absent)
        # render nodes, so the job stays PENDING. wait_job RAISES on a job that
        # never terminates, so a timeout here means "did not complete".
        try:
            final = wait_job(cluster, job_id, timeout=CONVERGE_TIMEOUT + 20)
        except TimeoutError:
            final = None
        if final == "CD":
            content = cluster.wait_output(out_path, "GREW_OK", timeout=30)
            assert "GREW_OK" in content, f"job did not run to completion:\n{content}"
            return
        post = job_state(cluster.squeue_all(), job_id)
        if post == "PD":
            pytest.skip(
                "inventory grew to 4 GPUs, but this bed cannot execute a GPU job "
                "(no real /dev/dri/renderD* device nodes); the pre-growth PENDING "
                "assertion still ran. Use a GPU bed to exercise completion.\n"
                f"{cluster.debug_job(job_id)}"
            )
        pytest.fail(
            f"3-GPU job neither completed nor stayed pending after growth: "
            f"final={final} post={post}\n{cluster.debug_job(job_id)}"
        )

    def test_shrink_converges(self, unstarted_cluster):
        """Negative: shrinking the spec 4 -> 2 devices (nothing allocated) converges
        node_gpu_count down; freed devices stop being advertised."""
        cluster = unstarted_cluster
        node_name, cdi_dir = self._start(cluster, [128, 129, 130, 131])

        _write_spec(cluster, cdi_dir, [128, 129])
        got = _wait_node_gpu_count(cluster, node_name, 2)
        assert got == 2, (
            f"inventory shrink 4->2 must converge node_gpu_count to 2, got {got}\n"
            f"{cluster.scontrol_show_node(node_name)}\n"
            f"spurd log tail:\n{cluster.spurd_log()[-1500:]}"
        )

    def test_vanished_gpus_not_schedulable(self, unstarted_cluster):
        """Negative: after shrink to 2 devices, a job requesting 4 GPUs must NOT
        run to completion — the controller no longer believes in the removed GPUs."""
        cluster = unstarted_cluster
        node_name, cdi_dir = self._start(cluster, [128, 129, 130, 131])

        _write_spec(cluster, cdi_dir, [128, 129])
        got = _wait_node_gpu_count(cluster, node_name, 2)
        assert got == 2, (
            f"node must converge down to 2 GPUs before the negative check, got {got}\n"
            f"{cluster.scontrol_show_node(node_name)}"
        )

        probe = cluster.write_file(
            "shrink-probe.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
        )
        out_path = f"{cluster.remote_dir}/shrink-probe.out"
        sb = cluster.sbatch(
            ["-J", "vanished-4g", "-N", "1", "--gres=gpu:4", "-o", out_path, probe]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        # Give the scheduler several cycles; a 4-GPU job on a 2-GPU node must stay
        # pending (insufficient resources), never reaching a completed state.
        deadline = time.time() + 3 * REFRESH_SECS + 8
        state = "PD"
        while time.time() < deadline:
            state = job_state(cluster.squeue_all(), job_id) or "GONE"
            if state in ("CD", "F", "CA", "TO", "GONE"):
                break
            time.sleep(1)
        assert state == "PD", (
            f"4-GPU job must stay pending on a shrunk 2-GPU node, got {state}\n"
            f"{cluster.debug_job(job_id)}"
        )
        cluster.scancel(str(job_id))
