# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for GPU stable_id changes (re-encoding and repartition) under
running jobs, on the static-CDI harness of test_inventory_convergence.py.
"""

import json
import re
import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state
from test_inventory_convergence import (
    CONVERGE_TIMEOUT,
    GPU_TYPE,
    REFRESH_SECS,
    _assert_count_stays,
    _encode_stable_id,
    _find_descriptor_paths,
    _spec_json,
    _wait_node_gpu_count,
    _write_spec_body,
)

pytestmark = pytest.mark.gpu

BASELINE_FILE = "inventory-baseline.json"

# Several scheduler cycles: long enough that a job which could run would.
PENDING_WINDOW = 3 * REFRESH_SECS + 6


def _device(idx: int, minor: int, sid: int) -> dict:
    return {
        "name": str(idx),
        "annotations": {
            "spur.amd.com/render-minor": str(minor),
            "spur.amd.com/stable-id": str(sid),
            "spur.amd.com/gpu-type": GPU_TYPE,
        },
        "containerEdits": {"deviceNodes": [{"path": f"/dev/dri/renderD{minor}"}]},
    }


def _legacy_cpx(minors: list[int], bus: int = 0x11) -> list[dict]:
    """One GPU in len(minors) partitions, rank in the PCI function bits."""
    return [_device(i, m, _encode_stable_id(bus, 0, i, 0)) for i, m in enumerate(minors)]


def _layout(minors: list[int], bus: int = 0x11) -> list[dict]:
    """One GPU in len(minors) partitions, current encoding."""
    return [_device(i, m, _encode_stable_id(bus, 0, 0, i)) for i, m in enumerate(minors)]


def _spx(minors: list[int]) -> list[dict]:
    """One unpartitioned GPU per minor, on buses 0x20, 0x21, ..."""
    return [_device(i, m, _encode_stable_id(0x20 + i, 0, 0, 0)) for i, m in enumerate(minors)]


def _ids(devices: list[dict]) -> set[int]:
    return {int(d["annotations"]["spur.amd.com/stable-id"]) for d in devices}


def _render_minors(cluster, need: int = 2) -> list[int]:
    """The bed's real render minors on node 0; skips below *need*."""
    cluster.require_nodes(1)
    cluster.gpu_preflight(1)
    out = cluster.nodes[0].exec_allow_fail("ls /dev/dri 2>/dev/null")
    minors = sorted(int(m) for m in re.findall(r"\brenderD(\d+)\b", out))
    if len(minors) < need:
        pytest.skip(f"need {need} render nodes on node 0, found {minors}")
    return minors


def _start(cluster, devices: list[dict]) -> tuple[str, str]:
    """Start with a static CDI spec of *devices*. Returns (node_name, cdi_dir).

    Skips if the seeded spec is not advertised: nothing below would then test
    the identity handling."""
    cdi_dir = f"{cluster.remote_dir}/cdi"
    cluster.nodes[0].exec(f"mkdir -p '{cdi_dir}'")
    _write_spec_body(cluster, cdi_dir, _spec_json(devices))
    cluster.agent_env = {"SPUR_INVENTORY_REFRESH_SECS": str(REFRESH_SECS)}
    cluster.start(
        config_overrides=cluster.devices_config(
            auto_detect=False, cdi_spec_dirs=[cdi_dir]
        )
    )
    node_name = cluster.node_names[0]
    seeded = _wait_node_gpu_count(cluster, node_name, len(devices))
    if seeded != len(devices):
        pytest.skip(
            f"static CDI did not advertise {len(devices)} GPUs (got {seeded})\n"
            f"{cluster.scontrol_show_node(node_name)}"
        )
    return node_name, cdi_dir


def _baseline_path(cluster) -> str:
    return f"{cluster.state_dir}/runtime/{BASELINE_FILE}"


def _reported_ids(cluster) -> set[int] | None:
    raw = cluster.nodes[0].exec_allow_fail(f"cat '{_baseline_path(cluster)}' 2>/dev/null")
    try:
        doc = json.loads(raw)
    except ValueError:
        return None
    return {gpu["stable_id"] for gpu in doc.get("gpus", [])}


def _assert_reported_ids(cluster, expected: set[int], why: str) -> None:
    """Wait for spurd's acknowledged inventory to carry exactly *expected*."""
    deadline = time.time() + CONVERGE_TIMEOUT
    got = _reported_ids(cluster)
    while got != expected and time.time() < deadline:
        time.sleep(1)
        got = _reported_ids(cluster)
    assert got == expected, (
        f"{why}: reported ids must be {sorted(map(hex, expected))}, got "
        f"{sorted(map(hex, got)) if got is not None else None}\n"
        f"spurd log tail:\n{cluster.spurd_log()[-2000:]}"
    )


def _held_gpu_ids(cluster, job_id: int) -> list[int] | None:
    """The gpu ids recorded in the job's stepd descriptor(s), once one exists."""
    deadline = time.time() + 20
    while time.time() < deadline:
        paths = _find_descriptor_paths(cluster, job_id)
        if paths:
            held = set()
            for path in paths:
                doc = json.loads(cluster.nodes[0].read_file(path))
                held.update(doc["resources"].get("gpu_devices", []))
            return sorted(held)
        time.sleep(1)
    return None


def _submit(cluster, jobs: list[int], node_name: str, name: str, gpus: int,
            body: str, out_path: str | None = None) -> int:
    script = cluster.write_file(f"{name}.sh", f"#!/bin/bash\n{body}\n", all_nodes=True)
    args = ["-J", name, "-N", "1", "-w", node_name, f"--gres=gpu:{gpus}"]
    if out_path:
        args += ["-o", out_path]
    job_id = parse_job_id(cluster.sbatch(args + [script]))
    assert job_id is not None, f"sbatch {name} failed"
    jobs.append(job_id)
    return job_id


def _hold(cluster, jobs: list[int], node_name: str, want: int, slots: int) -> int:
    """A running gpu:1 job holding exactly *want*.

    The scheduler takes the lowest free device, so fill devices one job at a
    time until one lands on *want*, then release the fillers."""
    fillers = []
    for n in range(slots):
        job_id = _submit(cluster, jobs, node_name, f"hold-{n}", 1, "sleep 300")
        wait_job_state(cluster, job_id, "R")
        held = _held_gpu_ids(cluster, job_id)
        if held == [want]:
            for filler in fillers:
                cluster.scancel(str(filler))
                wait_job(cluster, filler, timeout=60)
            return job_id
        fillers.append(job_id)
    pytest.fail(
        f"no gpu:1 job landed on {want:#x} after {slots} tries\n"
        f"{cluster.scontrol_show_node(node_name)}"
    )


def _assert_stays_pending(cluster, jobs: list[int], node_name: str, gpus: int,
                          why: str) -> None:
    job_id = _submit(cluster, jobs, node_name, f"pending-{gpus}g", gpus, "echo SHOULD_NOT_RUN")
    deadline = time.time() + PENDING_WINDOW
    state = "PD"
    while time.time() < deadline:
        state = job_state(cluster.squeue_all(), job_id) or "GONE"
        if state != "PD":
            break
        time.sleep(1)
    cluster.scancel(str(job_id))
    assert state == "PD", f"{why}; got {state}\n{cluster.debug_job(job_id)}"


def _assert_completes(cluster, jobs: list[int], node_name: str, gpus: int,
                      why: str) -> None:
    out_path = f"{cluster.remote_dir}/probe-{gpus}g-{len(jobs)}.out"
    job_id = _submit(
        cluster, jobs, node_name, f"probe-{gpus}g", gpus, "echo PROBE_OK", out_path
    )
    try:
        final = wait_job(cluster, job_id, timeout=CONVERGE_TIMEOUT + 40)
    except TimeoutError:
        final = job_state(cluster.squeue_all(), job_id)
    assert final == "CD", f"{why}; got {final}\n{cluster.debug_job(job_id)}"
    content = cluster.wait_output(out_path, "PROBE_OK", timeout=30)
    assert "PROBE_OK" in content, f"{why}: no output\n{content}"


def _assert_no_dispatch_rejected(cluster) -> None:
    log = cluster.spurd_log()
    assert "rejecting dispatch" not in log, (
        f"spurd rejected a dispatch: controller and agent disagree on gpu ids\n"
        f"spurd log tail:\n{log[-2500:]}"
    )


def _restart_with_spec(cluster, cdi_dir: str, devices: list[dict],
                       keep_baseline: bool) -> None:
    """Rewrite the spec while spurd is down, so the change is seen only at
    startup, then bring every agent back."""
    cluster.stop_agents()
    _write_spec_body(cluster, cdi_dir, _spec_json(devices))
    if not keep_baseline:
        cluster.nodes[0].exec(f"rm -f '{_baseline_path(cluster)}'")
    for index in range(len(cluster.nodes)):
        cluster.restart_agent(index)
    cluster.wait_agent_serving(0)


def _cancel_all(cluster, jobs: list[int]) -> None:
    for job_id in jobs:
        cluster.scancel(str(job_id))


class TestEncodingChange:
    """The same partitions re-announced under the current id encoding."""

    def _seed_and_hold(self, cluster, jobs):
        minors = _render_minors(cluster)[:2]
        node_name, cdi_dir = _start(cluster, _legacy_cpx(minors))
        legacy_rank1 = _encode_stable_id(0x11, 0, 1, 0)
        hold = _hold(cluster, jobs, node_name, legacy_rank1, slots=2)
        return minors, node_name, cdi_dir, hold

    def _assert_held_and_unfenced(self, cluster, jobs, node_name, hold):
        assert job_state(cluster.squeue_all(), hold) == "R", (
            f"the holding job must still be RUNNING\n{cluster.debug_job(hold)}"
        )
        _assert_count_stays(cluster, node_name, 2, window=2 * REFRESH_SECS + 4)
        # Completing proves the free partition is offered and the controller
        # moved the hold onto its new id rather than withholding the node.
        _assert_completes(
            cluster, jobs, node_name, 1,
            "a gpu:1 job must run on the free partition after re-encoding",
        )
        _assert_stays_pending(
            cluster, jobs, node_name, 2,
            "a gpu:2 job must stay pending while the renamed partition is held",
        )
        _assert_no_dispatch_rejected(cluster)

    def test_reencoded_partitions_stay_with_the_running_job(self, unstarted_cluster):
        cluster = unstarted_cluster
        jobs: list[int] = []
        try:
            minors, node_name, cdi_dir, hold = self._seed_and_hold(cluster, jobs)
            current = _layout(minors)

            _write_spec_body(cluster, cdi_dir, _spec_json(current))
            _assert_reported_ids(cluster, _ids(current), "re-encoding on refresh")

            self._assert_held_and_unfenced(cluster, jobs, node_name, hold)
            new_rank1 = _encode_stable_id(0x11, 0, 0, 1)
            assert _held_gpu_ids(cluster, hold) == [new_rank1], (
                "the running job's descriptor must record the partition's new id"
            )
        finally:
            _cancel_all(cluster, jobs)

    @pytest.mark.parametrize("keep_baseline", [False, True],
                             ids=["without-baseline", "with-baseline"])
    def test_reencoded_partitions_survive_a_restart(
        self, unstarted_cluster, keep_baseline
    ):
        cluster = unstarted_cluster
        jobs: list[int] = []
        try:
            minors, node_name, cdi_dir, hold = self._seed_and_hold(cluster, jobs)
            current = _layout(minors)

            _restart_with_spec(cluster, cdi_dir, current, keep_baseline)
            log = cluster.spurd_log()
            assert "could not restore an adopted job's allocation" not in log, (
                f"the adopted job's gpu must map onto its new id\n{log[-2500:]}"
            )
            _assert_reported_ids(cluster, _ids(current), "re-encoding across restart")

            self._assert_held_and_unfenced(cluster, jobs, node_name, hold)
        finally:
            _cancel_all(cluster, jobs)


class TestPartitionChange:
    """A GPU's partition set changes, with and without a job holding it."""

    @pytest.mark.parametrize("to_cpx", [True, False], ids=["spx-to-cpx", "cpx-to-spx"])
    def test_change_with_no_jobs_adopts_the_new_layout(self, unstarted_cluster, to_cpx):
        cluster = unstarted_cluster
        jobs: list[int] = []
        try:
            minors = _render_minors(cluster)[:2]
            # Bus 0x20 is the repartitioned GPU on both sides; 0x21 exists only
            # in the SPX layout.
            spx, cpx = _spx(minors), _layout(minors, bus=0x20)
            before, after = (spx, cpx) if to_cpx else (cpx, spx)
            node_name, cdi_dir = _start(cluster, before)

            _write_spec_body(cluster, cdi_dir, _spec_json(after))
            _assert_reported_ids(cluster, _ids(after), "unheld repartition")
            assert _wait_node_gpu_count(cluster, node_name, 2) == 2

            _assert_completes(
                cluster, jobs, node_name, 2,
                "a gpu:2 job must run on the adopted layout",
            )
            _assert_no_dispatch_rejected(cluster)
        finally:
            _cancel_all(cluster, jobs)

    def test_more_partitions_under_a_held_gpu_offer_none_of_it(self, unstarted_cluster):
        cluster = unstarted_cluster
        jobs: list[int] = []
        try:
            minors = _render_minors(cluster)[:2]
            node_name, cdi_dir = _start(cluster, _spx(minors))
            held = _encode_stable_id(0x20, 0, 0, 0)
            hold = _hold(cluster, jobs, node_name, held, slots=2)

            # Bus 0x20 splits in two; rank 0 reuses the held SPX id, so only the
            # layout change tells the held device is not the same partition.
            # Bus 0x21 disappears.
            cpx = _layout(minors, bus=0x20)
            _write_spec_body(cluster, cdi_dir, _spec_json(cpx))
            _assert_reported_ids(cluster, {held}, "repartition of a held gpu")
            _assert_count_stays(cluster, node_name, 1, window=2 * REFRESH_SECS + 4)
            assert job_state(cluster.squeue_all(), hold) == "R", (
                f"the holding job must still be RUNNING\n{cluster.debug_job(hold)}"
            )
            _assert_stays_pending(
                cluster, jobs, node_name, 1,
                "no partition of the held gpu may be offered while it is held",
            )

            cluster.scancel(str(hold))
            wait_job(cluster, hold, timeout=60)
            _assert_reported_ids(cluster, _ids(cpx), "released gpu adopts its layout")
            assert _wait_node_gpu_count(cluster, node_name, 2) == 2
            _assert_completes(
                cluster, jobs, node_name, 2,
                "a gpu:2 job must run once the held gpu is released",
            )
            _assert_no_dispatch_rejected(cluster)
        finally:
            _cancel_all(cluster, jobs)

    def test_change_while_stopped_fences_the_held_gpu(self, unstarted_cluster):
        cluster = unstarted_cluster
        jobs: list[int] = []
        try:
            minors = _render_minors(cluster)[:2]
            node_name, cdi_dir = _start(cluster, _layout(minors, bus=0x20))
            held = _encode_stable_id(0x20, 0, 0, 1)
            hold = _hold(cluster, jobs, node_name, held, slots=2)

            spx = _spx(minors[:1])
            _restart_with_spec(cluster, cdi_dir, spx, keep_baseline=True)
            assert job_state(cluster.squeue_all(), hold) == "R", (
                f"the holding job must survive the restart\n{cluster.debug_job(hold)}"
            )
            _assert_reported_ids(cluster, {held}, "repartition seen at startup")
            _assert_count_stays(cluster, node_name, 1, window=2 * REFRESH_SECS + 4)
            _assert_stays_pending(
                cluster, jobs, node_name, 1,
                "the fresh SPX partition must not be offered while the gpu is held",
            )

            cluster.scancel(str(hold))
            wait_job(cluster, hold, timeout=60)
            # The count stays 1 across the swap; only the id shows the adoption.
            _assert_reported_ids(cluster, _ids(spx), "released gpu adopts SPX")
            assert _wait_node_gpu_count(cluster, node_name, 1) == 1
            _assert_completes(
                cluster, jobs, node_name, 1,
                "a gpu:1 job must run on the adopted SPX partition",
            )
            _assert_no_dispatch_rejected(cluster)
        finally:
            _cancel_all(cluster, jobs)
