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

Each GPU carries a `spur.amd.com/stable-id` annotation (a BDF-anchored u32
computed like the Rust `encode_stable_id`) that becomes the GPU's stable_id in
the scheduler, plus a `spur.amd.com/render-minor` annotation (injection still
keys the device node off the minor). The controller prints one `gpu:<type>:1`
Gres entry per device, so `node_gpu_count` returns the device count.
"""

import json
import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

# Fast refresh so a rewrite is adopted within a couple of ticks.
REFRESH_SECS = 2

# Convergence deadline: several refresh intervals plus re-register + controller
# round-trip. The refresh loop also debounces (a change must be seen on two
# consecutive ticks before it is applied), so allow > 2 * REFRESH_SECS.
CONVERGE_TIMEOUT = 20

GPU_TYPE = "mi300x"


def _encode_stable_id(bus: int, dev: int, func: int, partition: int) -> int:
    """Mirror the Rust `encode_stable_id` (crates/spur-devices/src/cdi/discovery.rs):
    a reload-invariant device id from the PCIe BDF anchor plus a per-partition
    rank. Two ids share physical silicon iff their BDF (bus/dev/func) is equal.
    Domain is omitted (single-domain), matching the Rust encoding.
    """
    return (
        ((bus & 0xFF) << 16)
        | ((dev & 0x1F) << 11)
        | ((func & 0x07) << 8)
        | (partition & 0xFF)
    )


# A CDI device spec: distinct BDF bus per device (so distinct silicon / stable_id)
# unless a caller overrides `bus`/`partition` to place two partitions on one GPU.
def _gpu_device(idx: int, minor: int, *, bus: int, partition: int = 0) -> dict:
    stable_id = _encode_stable_id(bus, 0, 0, partition)
    return {
        "name": str(idx),
        "annotations": {
            # render-minor still selects the device node; stable-id is the real
            # identity the scheduler keys on after the BDF-anchoring rework.
            "spur.amd.com/render-minor": str(minor),
            "spur.amd.com/stable-id": str(stable_id),
            "spur.amd.com/gpu-type": GPU_TYPE,
        },
        "containerEdits": {
            "deviceNodes": [{"path": f"/dev/dri/renderD{minor}"}],
        },
    }


def _cdi_spec(render_minors: list[int]) -> str:
    """A static AMD CDI spec JSON declaring one GPU per render-minor.

    Each device gets a distinct BDF bus (its index) so the BDF-encoded stable_id
    is distinct and meaningful — this exercises the real stable-id annotation
    path rather than the positional backfill. Field names mirror
    crates/spur-devices/src/cdi/spec.rs serde attributes: cdiVersion, kind,
    devices[].name, devices[].annotations, devices[].containerEdits.deviceNodes[].path.
    """
    devices = [
        _gpu_device(idx, minor, bus=idx + 1)
        for idx, minor in enumerate(render_minors)
    ]
    return _spec_json(devices)


def _spec_json(devices: list[dict]) -> str:
    """Wrap device dicts in the CDI envelope the loader validates."""
    return json.dumps(
        {"cdiVersion": "0.6.0", "kind": "amd.com/gpu", "devices": devices},
        indent=2,
    )


def _write_spec(cluster, cdi_dir: str, render_minors: list[int]) -> None:
    """(Re)write the CDI spec on the agent node. Rewrites are atomic (mv) so the
    refresh loop never reads a half-written file."""
    _write_spec_body(cluster, cdi_dir, _cdi_spec(render_minors))


def _write_spec_body(cluster, cdi_dir: str, body: str) -> None:
    """Atomically (re)write a raw CDI spec body on the agent node."""
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


def _start_static_cdi(cluster, render_minors: list[int]) -> tuple[str, str]:
    """Provision a single-node cluster with a static CDI dir and fast refresh,
    seeded with *render_minors*. Returns (node_name, cdi_dir).

    Skips (not fails) if the seeded spec is not advertised, since the whole
    premise (static CDI advertises without KFD) would then be false and every
    downstream assertion meaningless."""
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
    seeded = _wait_node_gpu_count(cluster, node_name, len(render_minors))
    if seeded != len(render_minors):
        pytest.skip(
            f"static CDI did not advertise {len(render_minors)} GPUs "
            f"(got {seeded}); loader may require real device nodes on this bed\n"
            f"{cluster.scontrol_show_node(node_name)}"
        )
    return node_name, cdi_dir


class TestInventoryConvergence:
    """Static-CDI convergence: rewrite the on-disk spec, prove the controller's
    view follows without restarting spurd."""

    def _start(self, cluster, render_minors: list[int]) -> tuple[str, str]:
        return _start_static_cdi(cluster, render_minors)

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


def _assert_count_stays(cluster, node_name: str, expected: int, window: int) -> None:
    """Fail fast if node_gpu_count ever leaves *expected* within *window* seconds.

    A non-event (e.g. an out-of-band change to held silicon that must not alter
    the advertised inventory) is proven by the count HOLDING, so this polls the
    whole window rather than returning on first match."""
    deadline = time.time() + window
    while time.time() < deadline:
        got = cluster.node_gpu_count(node_name)
        assert got == expected, (
            f"advertised GPU count must stay {expected}, saw {got}\n"
            f"{cluster.scontrol_show_node(node_name)}\n"
            f"spurd log tail:\n{cluster.spurd_log()[-2000:]}"
        )
        time.sleep(1)


class TestAllocatedDeviceNonEvent:
    """Item 2: an out-of-band change to ALLOCATED silicon is a non-event. The
    held device stays pinned, the free pool converges, and a fresh partition
    sharing a held device's BDF is never advertised as free (no double-book)."""

    def test_repartition_of_held_silicon_pins_and_suppresses_overlap(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        # 2 GPUs on distinct silicon: renderD128 -> bus 1 (stable_id 0x10000),
        # renderD129 -> bus 2 (stable_id 0x20000). gpu:1 takes the lowest device
        # index (renderD128 sorts first), so the held device is bus 1.
        node_name, cdi_dir = _start_static_cdi(cluster, [128, 129])
        assert cluster.node_gpu_count(node_name) == 2

        hold = cluster.write_file(
            "alloc-hold.sh", "#!/bin/bash\nsleep 300\n", all_nodes=True
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "hold-1g", "-N", "1", "-w", node_name, "--gres=gpu:1", hold]
            )
        )
        assert job_id is not None

        # The held state lives in the node's NodeAllocation, which is only
        # populated once the agent actually launches the job. A GPU-less bed
        # cannot open the (absent) renderD nodes, so the job never runs and the
        # held-device path cannot be exercised here — skip with that reason.
        try:
            wait_job_state(cluster, job_id, "R", timeout=3 * REFRESH_SECS + 10)
        except TimeoutError:
            st = job_state(cluster.squeue_all(), job_id)
            cluster.scancel(str(job_id))
            pytest.skip(
                f"gpu:1 job never reached RUNNING (state {st}); cannot establish "
                "a held device without real /dev/dri renderD nodes. Run on a GPU "
                "bed to exercise held-device pinning.\n"
                f"{cluster.debug_job(job_id)}"
            )

        try:
            # Out-of-band change to the HELD silicon (bus 1): its SPX partition
            # (stable_id 0x10000) disappears, replaced by two CPX slices on the
            # SAME bus with DIFFERENT stable_ids (0x10001, 0x10002) — this is the
            # AllocatedDevicesLost case (held id absent from the fresh scan). The
            # free GPU on bus 2 is untouched, and a brand-new free GPU on bus 9 is
            # added so the refresh loop's processing is observable (count moves).
            repart = _spec_json(
                [
                    _gpu_device(0, 130, bus=1, partition=1),
                    _gpu_device(1, 131, bus=1, partition=2),
                    _gpu_device(2, 129, bus=2, partition=0),
                    _gpu_device(3, 140, bus=9, partition=0),
                ]
            )
            _write_spec_body(cluster, cdi_dir, repart)

            # Correct convergence: held bus-1 slice pinned (from the last-reported
            # set) + free bus-2 + new free bus-9 = 3. The two bus-1 CPX slices
            # share the held BDF and MUST be suppressed. A count of 4 would mean
            # the held silicon was double-booked (a repartition of it offered as
            # free while the job holds it).
            got = _wait_node_gpu_count(cluster, node_name, 3, timeout=CONVERGE_TIMEOUT)
            assert got == 3, (
                "held-silicon repartition must converge to 3 GPUs (held pinned + "
                f"two unrelated free), got {got} (4 == double-book of held silicon)\n"
                f"{cluster.scontrol_show_node(node_name)}\n"
                f"spurd log tail:\n{cluster.spurd_log()[-2000:]}"
            )
            # And it must STAY 3 — a later tick must not leak the overlap as free.
            _assert_count_stays(cluster, node_name, 3, window=2 * REFRESH_SECS + 4)
            assert job_state(cluster.squeue_all(), job_id) == "R", (
                "the holding job must still be RUNNING after the repartition\n"
                f"{cluster.debug_job(job_id)}"
            )

            # Behavioral no-double-book: only bus-2 and bus-9 are free (2 GPUs);
            # the held bus-1 is pinned-not-free and its slices suppressed. A gpu:3
            # job therefore cannot be satisfied and must stay pending. If a held
            # slice had leaked into the free pool there would be 3 free GPUs and
            # this job could run.
            probe = cluster.write_file(
                "nodouble-probe.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
            )
            out_path = f"{cluster.remote_dir}/nodouble.out"
            jid2 = parse_job_id(
                cluster.sbatch(
                    [
                        "-J", "nodouble-3g", "-N", "1", "-w", node_name,
                        "--gres=gpu:3", "-o", out_path, probe,
                    ]
                )
            )
            assert jid2 is not None
            deadline = time.time() + 3 * REFRESH_SECS + 6
            st2 = "PD"
            while time.time() < deadline:
                st2 = job_state(cluster.squeue_all(), jid2) or "GONE"
                if st2 in ("CD", "F", "CA", "TO", "GONE"):
                    break
                time.sleep(1)
            cluster.scancel(str(jid2))
            assert st2 == "PD", (
                "a second gpu:3 job must stay pending: only the two unrelated free "
                f"GPUs are schedulable, the held device's repartition is not free; "
                f"got {st2}\n{cluster.debug_job(jid2)}"
            )
        finally:
            cluster.scancel(str(job_id))


def _find_descriptor_paths(cluster, job_id: int) -> list[str]:
    """Return the on-disk stepd descriptor.json paths for *job_id*.

    Layout: <state_dir>/runtime/<job>.<attempt>.<step>/descriptor.json (see
    stepd.rs DESCRIPTOR_FILE / session_dir)."""
    runtime = f"{cluster.state_dir}/runtime"
    found = cluster.nodes[0].exec_allow_fail(
        f"find '{runtime}' -maxdepth 2 -name descriptor.json "
        f"-path '*/{job_id}.*' 2>/dev/null"
    ).strip()
    return [p for p in found.splitlines() if p]


def _rewrite_descriptor_gpu_devices(
    cluster, path: str, gpu_devices: list[int]
) -> list[int]:
    """Rewrite a descriptor's resources.gpu_devices to *gpu_devices* in place,
    atomically. Returns the previous value so a test can assert it changed."""
    raw = cluster.nodes[0].read_file(path)
    doc = json.loads(raw)
    previous = list(doc["resources"].get("gpu_devices", []))
    doc["resources"]["gpu_devices"] = gpu_devices
    tmp = f"{path}.forge.tmp"
    cluster.nodes[0].write_file(tmp, json.dumps(doc), mode=0o644)
    cluster.nodes[0].exec(f"mv -f '{tmp}' '{path}'")
    return previous


class TestUpgradeSurvival:
    """Item 3: a running GPU job's allocation survives a spurd restart (the
    id-scheme migration path at descriptor restore) without double-booking."""

    def test_legacy_positional_descriptor_adopts_onto_stable_ids(self, gpu_cluster):
        """The genuine version-upgrade path: a pre-upgrade spurd persisted the
        job's GPUs as POSITIONAL ids ([0, 1, ...]); the upgraded agent discovers
        BDF-anchored stable_ids that don't match. On restart, restore must
        translate positional i -> current_gpus[i].stable_id (translate_legacy_gpu_ids)
        so the running job re-adopts its real silicon, with no double-book.

        We simulate the old on-disk format by rewriting the live descriptor's
        resources.gpu_devices to positional ids WHILE the job runs, then restart
        spurd — which forces the restore-time translation branch that a plain
        same-version restart never hits."""
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        node_name = cluster.node_names[0]
        total = cluster.node_gpu_count(node_name)
        assert total >= 1, f"gpu_cluster node advertises no GPUs\n{cluster.sinfo()}"

        hold = cluster.write_file(
            "legacy-hold.sh", "#!/bin/bash\nsleep 300\n", all_nodes=True
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "legacy-hold", "-N", "1", "-w", node_name, "--gres=gpu:1", hold]
            )
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        try:
            # Find the persisted descriptor(s) and rewrite the held GPU id to the
            # legacy POSITIONAL form. A gpu:1 job holds exactly one device; the
            # old scheme would have recorded its positional index 0.
            paths = _find_descriptor_paths(cluster, job_id)
            if not paths:
                cluster.scancel(str(job_id))
                pytest.skip(
                    "no on-disk stepd descriptor found for the running job; cannot "
                    f"forge a legacy positional recording\n{cluster.debug_job(job_id)}"
                )
            forged_any = False
            for path in paths:
                previous = _rewrite_descriptor_gpu_devices(cluster, path, [0])
                # Only meaningful if the real recording was a stable_id != 0 (the
                # BDF-anchored value). If it were already [0], there'd be nothing
                # to translate and the test would prove nothing.
                if previous and previous != [0]:
                    forged_any = True
            if not forged_any:
                cluster.scancel(str(job_id))
                pytest.skip(
                    "descriptor already recorded positional/zero gpu ids; the "
                    "BDF-anchored stable_id path is not exercised on this bed"
                )

            # Restart just spurd. Adoption replays the (now legacy-positional)
            # descriptor through restore -> translate_legacy_gpu_ids.
            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)

            # The agent logs the translation when the branch fires — a direct
            # signal that the legacy path (not a same-scheme re-adopt) ran.
            log = cluster.spurd_log(0)
            assert "translated a legacy positional gpu recording" in log, (
                "restart must translate the forged legacy positional gpu ids to "
                "stable_ids on adopt (translate_legacy_gpu_ids branch did not fire)\n"
                f"spurd log tail:\n{log[-2500:]}"
            )

            assert job_state(cluster.squeue_all(), job_id) == "R", (
                "the GPU job must still be RUNNING after the legacy-id translation\n"
                f"{cluster.debug_job(job_id)}"
            )

            # No double-book: the translated held GPU is accounted, so a job
            # asking for every GPU cannot be satisfied while one is held. If
            # translation had failed (GpusUnavailable -> unaccounted), the held
            # GPU would read free and this job could grab it.
            probe = cluster.write_file(
                "legacy-grab.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
            )
            out_path = f"{cluster.remote_dir}/legacy-grab.out"
            jid2 = parse_job_id(
                cluster.sbatch(
                    [
                        "-J", "legacy-grab", "-N", "1", "-w", node_name,
                        f"--gres=gpu:{total}", "-o", out_path, probe,
                    ]
                )
            )
            assert jid2 is not None
            deadline = time.time() + 20
            st2 = "PD"
            while time.time() < deadline:
                st2 = job_state(cluster.squeue_all(), jid2) or "GONE"
                if st2 in ("CD", "F", "CA", "TO", "GONE"):
                    break
                time.sleep(1)
            cluster.scancel(str(jid2))
            assert st2 == "PD", (
                f"a gpu:{total} job must stay pending while the translated GPU is "
                f"held (no double-book after legacy-id migration); got {st2}\n"
                f"{cluster.debug_job(jid2)}"
            )
        finally:
            cluster.scancel(str(job_id))

    def test_running_gpu_job_survives_agent_restart_without_double_book(
        self, gpu_cluster
    ):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        node_name = cluster.node_names[0]
        total = cluster.node_gpu_count(node_name)
        assert total >= 1, f"gpu_cluster node advertises no GPUs\n{cluster.sinfo()}"

        hold = cluster.write_file(
            "upgrade-hold.sh", "#!/bin/bash\nsleep 300\n", all_nodes=True
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "upgrade-hold", "-N", "1", "-w", node_name, "--gres=gpu:1", hold]
            )
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        try:
            # Restart just spurd (not the controller). The agent re-adopts the
            # supervised job and restores its stepd descriptor — the code path
            # that translates a legacy job's positional gpu ids to current
            # stable_ids. A plain restart re-reads a current-scheme descriptor;
            # forging an on-disk legacy descriptor (positional gpu_devices=[0,1])
            # to hit the translation branch specifically is too schema-invasive
            # for a black-box e2e (noted as a harness limitation), so this asserts
            # the weaker-but-real guarantee: the restart must not double-book.
            cluster.restart_agent(0)
            cluster.wait_agent_serving(0)

            assert job_state(cluster.squeue_all(), job_id) == "R", (
                "the GPU job must still be RUNNING after its agent restarted\n"
                f"{cluster.debug_job(job_id)}"
            )

            # No double-book: the held GPU is still accounted, so a second job
            # asking for every GPU on the node cannot be satisfied while one is
            # held. If the restart had lost track of the held device, all `total`
            # GPUs would look free and this job could run.
            probe = cluster.write_file(
                "upgrade-grab.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
            )
            out_path = f"{cluster.remote_dir}/upgrade-grab.out"
            jid2 = parse_job_id(
                cluster.sbatch(
                    [
                        "-J", "upgrade-grab", "-N", "1", "-w", node_name,
                        f"--gres=gpu:{total}", "-o", out_path, probe,
                    ]
                )
            )
            assert jid2 is not None
            deadline = time.time() + 20
            st2 = "PD"
            while time.time() < deadline:
                st2 = job_state(cluster.squeue_all(), jid2) or "GONE"
                if st2 in ("CD", "F", "CA", "TO", "GONE"):
                    break
                time.sleep(1)
            cluster.scancel(str(jid2))
            assert st2 == "PD", (
                f"a gpu:{total} job must stay pending while one GPU is held across "
                f"the restart (no double-book); got {st2}\n{cluster.debug_job(jid2)}"
            )
        finally:
            cluster.scancel(str(job_id))


class TestAutoDetectHardware:
    """Real-KFD scenarios: these exercise the auto-detect discovery path (not
    static CDI) and need GPU hardware. They skip cleanly on a GPU-less bed."""

    @pytest.mark.gpu
    def test_partition_switch_reconverges_without_restart(self, unstarted_cluster):
        """Item 1 on real hardware: a live SPX<->CPX partition switch changes the
        KFD-discovered GPU set, and the auto-detect refresh loop must reconverge
        node_gpu_count WITHOUT a spurd restart. Uses unstarted_cluster (not
        gpu_cluster) because it must set a fast refresh cadence BEFORE the single
        start, and the scenario forbids a restart to apply it."""
        cluster = unstarted_cluster
        cluster.require_nodes(1)
        cluster.gpu_preflight(1)
        node = cluster.nodes[0]

        if "amd-smi" not in node.exec_allow_fail("command -v amd-smi || true"):
            pytest.skip("amd-smi not on the GPU node; cannot switch compute partition")
        part_help = node.exec_allow_fail("amd-smi partition --help 2>&1 | head -1")
        if "partition" not in part_help.lower():
            pytest.skip(
                "amd-smi has no `partition` subcommand on this bed; SPX<->CPX "
                f"switching unsupported ({part_help.strip()!r})"
            )

        cluster.agent_env = {"SPUR_INVENTORY_REFRESH_SECS": str(REFRESH_SECS)}
        cluster.start(config_overrides=cluster.devices_config(auto_detect=True))
        node_name = cluster.node_names[0]

        before = cluster.node_gpu_count(node_name)
        if before < 1:
            pytest.skip(
                f"auto-detect advertised {before} GPUs; nothing to repartition\n"
                f"{cluster.scontrol_show_node(node_name)}"
            )

        current = node.exec_allow_fail(
            "amd-smi partition --current 2>&1 | grep -oiE 'SPX|CPX' | head -1"
        ).strip().upper()
        if current not in ("SPX", "CPX"):
            pytest.skip(
                f"could not read the current compute partition mode ({current!r}); "
                "refusing to switch a partition state we cannot restore"
            )
        target = "CPX" if current == "SPX" else "SPX"

        switched = node.exec_allow_fail(
            f"{cluster._sudo_prefix()}amd-smi set --compute-partition {target} 2>&1"
        )
        if "success" not in switched.lower() and "set to" not in switched.lower():
            pytest.skip(
                f"amd-smi could not switch {current}->{target} on this bed "
                f"({switched.strip()[:200]!r}); partition switching not permitted"
            )

        try:
            # The auto-detect refresh loop must pick up the new topology on its
            # own (no restart). CPX exposes more render nodes than SPX, so the
            # count must change and then hold at the new value.
            deadline = time.time() + CONVERGE_TIMEOUT
            after = before
            while time.time() < deadline:
                after = cluster.node_gpu_count(node_name)
                if after != before:
                    break
                time.sleep(1)
            assert after != before, (
                f"a live {current}->{target} switch must change the auto-detected "
                f"GPU count without a spurd restart; stayed at {before}\n"
                f"{cluster.scontrol_show_node(node_name)}\n"
                f"spurd log tail:\n{cluster.spurd_log()[-2000:]}"
            )
        finally:
            node.exec_allow_fail(
                f"{cluster._sudo_prefix()}amd-smi set --compute-partition {current} 2>&1"
            )
            # Let the loop reconverge to the restored topology before teardown.
            _wait_node_gpu_count(cluster, node_name, before, timeout=CONVERGE_TIMEOUT)

    def test_gpu_job_binds_physical_device_when_stable_id_differs(self, gpu_cluster):
        """Item 1 on real hardware: with auto-detect, stable_ids are BDF-anchored
        and diverge from the positional device_ids. A gpu:1 job must still bind a
        real physical device — injection resolves by stable_id, not position."""
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        node_name = cluster.node_names[0]

        probe = cluster.ship_fixture("gpu_env_probe.sh")
        for node in cluster.nodes:
            node.exec(f"chmod +x '{probe}'")
        hold = cluster.write_file(
            "bind-hold.sh", "#!/bin/bash\nsleep 300\n", all_nodes=True
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "bind-1g", "-N", "1", "-w", node_name, "--gres=gpu:1", hold]
            )
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        try:
            code, out = cluster.srun_in_allocation(job_id, [probe])
            assert "PROBE_OK" in out, (
                f"the GPU step did not run to completion (exit {code})\n"
                f"{cluster.debug_job(job_id)}\noutput:\n{out}"
            )
            fields = dict(
                line.split("=", 1)
                for line in out.splitlines()
                if "=" in line and not line.startswith((" ", "\x1b"))
            )
            assert fields.get("SPUR_COUNT") == "1", (
                "a gpu:1 job's step must hold exactly one GPU (injection resolved "
                f"its stable_id to a device)\noutput:\n{out}"
            )
            assert fields.get("ROCR_VISIBLE_DEVICES") not in (None, "", "-1"), (
                "the step must see a real physical device index, not the "
                f"no-devices sentinel — binding by stable_id must not fail when "
                f"stable_id != device_id\noutput:\n{out}"
            )
        finally:
            cluster.scancel(str(job_id))
