# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Scheduling and discovery against generated sysfs; no accelerator execution."""

import pytest

POD = "00000000-0000-4000-8000-000000000001"
GIB = 1024**3


def test_all_reported_gpu_properties(sim):
    inventory = sim.inventory()
    assert len(inventory) == 4
    for index, (node, gpus) in enumerate(sorted(inventory.items())):
        assert node == f"sim-node-{index}"
        assert len(gpus) == 4
        for local, gpu in enumerate(gpus):
            domain, bus = local // 2 + 1, 4 + local % 2
            assert gpu["stable_id"] == (domain << 24) | (bus << 16)
            assert gpu["bdf"] == f"{domain:04x}:{bus:02x}:00.0"
            assert gpu["pci_root"] == f"pci{domain:04x}:00"
            assert gpu["numa"] == local // 2
            assert gpu["vram_bytes"] == [24, 192, 192, 288][local] * GIB
            assert gpu["type"] == "mi300x"
            assert gpu["physical_pod"] == POD
            assert gpu["vpod"] == ("15" if index < 2 else "16")
            assert gpu["endpoint"] == str(index * 4 + local)
            assert gpu["state"] == "active"
            assert gpu["transport"] == "UALoE"
            assert gpu["status"] == "observed"
            assert gpu["physical_capacity"] == 72
            assert gpu["fabric_capacity"] == 8
            endpoints = range(0, 8) if index < 2 else range(8, 16)
            assert set(gpu["active_endpoints"]) == {str(i) for i in endpoints}
            assert set(gpu["local_endpoints"]) == {str(index * 4 + i) for i in range(4)}


@pytest.mark.parametrize("expression,expected", [
    ("gpu.vram=192GiB", [1, 2]),
    ("gpu.vram!=192GiB", [0, 3]),
    ("gpu.vram>192GiB", [3]),
    ("gpu.vram>=192GiB", [1, 2, 3]),
    ("gpu.vram<192GiB", [0]),
    ("gpu.vram<=192GiB", [0, 1, 2]),
    ("gpu.vram>=192GB", [1, 2, 3]),
    ("gpu.vram_bytes=206158430208", [1, 2]),
    ("gpu.type=mi300x&gpu.vram>=192GiB", [1, 2, 3]),
    ("gpu.numa_node_id=0", [0, 1]),
    ("gpu.numa_node_id!=0", [2, 3]),
    ("gpu.numa_node_id>=1", [2, 3]),
    ("gpu.pcie_root_id=pci0001:00", [0, 1]),
    ("gpu.pcie_root_id!=pci0001:00", [2, 3]),
    (f"gpu.physical_pod_id={POD}", [0, 1, 2, 3]),
    ("gpu.vpod_id=15", [0, 1, 2, 3]),
])
def test_property_predicates_select_only_matching_gpus(sim, expression, expected):
    inventory = sim.inventory()
    job = sim.submit(expression, gpus=len(expected))
    selected = sim.selected(job)
    assert len(selected) == 1
    node, ids = next(iter(selected.items()))
    assert set(ids) == {inventory[node][i]["stable_id"] for i in expected}
    if "vpod_id=15" in expression:
        assert node in {"sim-node-0", "sim-node-1"}
    result = sim.call("step", job, "-N", 1, "-n", 1, "hostname")
    assert node in result.stdout


@pytest.mark.parametrize("field", ["numa_node_id", "pcie_root_id"])
def test_same_property_selects_one_local_group(sim, field):
    job = sim.submit(f"same(gpu.{field})", gpus=2)
    selected = sim.selected(job)
    inventory = sim.inventory()
    node, ids = next(iter(selected.items()))
    key = "numa" if field == "numa_node_id" else "pci_root"
    assert len({g[key] for g in inventory[node] if g["stable_id"] in ids}) == 1
    assert len(ids) == 2


@pytest.mark.parametrize("field", ["numa_node_id", "pcie_root_id"])
def test_equal_local_labels_on_different_nodes_do_not_satisfy_same(sim, field):
    job = sim.submit(f"same(gpu.{field})", nodes=2, gpus=4)
    sim.pending(job)


def test_vram_predicate_is_not_a_node_label(sim):
    job = sim.submit("gpu.vram>=192GiB", gpus=4)
    assert "GpuPropertiesUnavailable" in sim.pending(job)


def test_gpu_type_contradiction_is_unschedulable(sim):
    job = sim.submit("gpu.type!=mi300x", gpus=1)
    sim.pending(job)


@pytest.mark.parametrize("hint", ["block", "tree", "none"])
def test_property_selection_can_span_vpods_unless_constrained(sim, hint):
    job = sim.submit("gpu.vram>=192GiB", nodes=4, gpus=12, extra=["--topology", hint])
    assert len(sim.selected(job)) == 4
    sim.cancel(job)
    job = sim.submit("gpu.vram>=192GiB&same(gpu.vpod_id)", nodes=4, gpus=12,
                     extra=["--topology", hint])
    sim.pending(job)


@pytest.mark.parametrize("extra", [["--gpus-per-node", "2"], ["--gpus-per-task", "2"], ["--gres", "gpu:mi300x:2"]])
def test_spec_gpu_quantity_forms(sim, extra):
    result = sim.cli("submit", "--parsable", "-N", 2, "-n", 2, *extra,
                     "--constraint", "gpu.type=mi300x&gpu.vram>=192GiB&same(gpu.vpod_id)",
                     "-t", "00:02:00", "--wrap", "sleep 120")
    job = int(result.stdout.strip())
    sim.jobs.append(job)
    selected = sim.selected(job)
    assert len(selected) == 2 and all(len(ids) == 2 for ids in selected.values())
    assert set(selected) <= {"sim-node-0", "sim-node-1"}


@pytest.mark.parametrize("expression", [
    "gpu.vram=18446744073709551615GiB", "gpu.type>mi300x", "gpu.numa_node_id=-1",
    "gpu.unknown=1", "same(gpu.pcie_root_id)|fast", "gpu.vram>=192GiB&",
])
def test_invalid_or_unsupported_property_grammar_is_rejected(sim, expression):
    result = sim.cli("submit", "--gpus", 1, "--constraint", expression, "--wrap", "true", check=False)
    assert result.returncode != 0


@pytest.mark.parametrize("flag", [
    "--segment=18", "--consolidate-segments", "--spread-segments", "--exclusive=topo",
    "--switches=1@10m", "--fabric-isolation=required", "--prefer=fast|large",
])
def test_unsupported_spec_block_and_managed_cli_is_explicitly_rejected(sim, flag):
    result = sim.cli("submit", flag, "--gpus", 1, "--wrap", "true", check=False)
    assert result.returncode != 0


@pytest.mark.parametrize("field,value,key", [
    ("type", "0x75a3", "type"),
    ("vram", "1073741824", "vram_bytes"),
    ("numa", "2", "numa"),
    ("numa", "-1", "numa"),
    ("vram", "malformed", "vram_bytes"),
    ("root", "changed", "pci_root"),
    ("ppod_id", "other-rack", "physical_pod"),
    ("vpod_id", "17", "vpod"),
    ("accel_state", "inactive", "state"),
    ("link_type", "changed", "transport"),
    ("accel_id", "99", "endpoint"),
    ("ppod_size", "71", "physical_capacity"),
    ("vpod_size", "7", "fabric_capacity"),
    ("vpod_active_accels", "0", "active_endpoints"),
    ("local_accels", "99", "local_endpoints"),
    ("vpod_id", "<missing>", "vpod"),
])
def test_changed_gpu_facts_reject_steps_with_old_binding(sim, field, value, key):
    node = "sim-node-0"
    before = sim.inventory()[node]
    job = sim.submit("gpu.vram>=24GiB", extra=["--nodelist", node])
    assert node in sim.selected(job)
    sim.call("step", job, "-N", 1, "-n", 1, "hostname")
    try:
        sim.call("mutate", node, field, value)
        sim.wait(lambda: sim.inventory()[node][0][key] != before[0][key])
        result = sim.call("step", job, "-N", 1, "-n", 1, "hostname", check=False)
        assert result.returncode != 0, result.stdout
        assert node not in result.stdout.splitlines()
    finally:
        sim.cancel(job)
        sim.call("reset")
        sim.wait(lambda: sim.inventory()[node] == before)


@pytest.mark.parametrize("field,value,expression", [
    ("numa", "-1", "gpu.numa_node_id!=0"),
    ("vram", "malformed", "gpu.vram!=192GiB"),
])
def test_unknown_values_do_not_satisfy_inequality(sim, field, value, expression):
    before = sim.inventory()["sim-node-0"]
    try:
        sim.call("mutate", "sim-node-0", field, value)
        key = "numa" if field == "numa" else "vram_bytes"
        sim.wait(lambda: sim.inventory()["sim-node-0"][0][key] is None)
        job = sim.submit(expression, extra=["--nodelist", "sim-node-0"])
        sim.pending(job)
        sim.cancel(job)
    finally:
        sim.call("reset")
        sim.wait(lambda: sim.inventory()["sim-node-0"] == before)


@pytest.mark.parametrize("verb", ["run", "alloc"])
def test_property_constraints_through_interactive_entry_points(sim, verb):
    args = [verb, "-N", 1, "-n", 1, "--gpus", 2, "--constraint",
            "gpu.vram>=192GiB&same(gpu.pcie_root_id)", "-t", "00:02:00"]
    result = sim.cli(*args, *(["--chdir", "/state", "hostname"] if verb == "run" else []),
                     stdin="spur run -N 1 -n 1 hostname\n" if verb == "alloc" else None)
    assert "sim-node-" in result.stdout


def test_property_constraints_with_optional_block_file(sim):
    before = sim.inventory()
    try:
        sim.call("block-file", "enable")
        job = sim.submit("gpu.vram>=192GiB&same(gpu.vpod_id)", nodes=2, gpus=6,
                         extra=["--topology", "block"])
        assert set(sim.selected(job)) == {"sim-node-0", "sim-node-1"}
        assert "BindingVersion=1" in sim.detail(job)
        sim.cancel(job)
        job = sim.submit("gpu.vram>=192GiB&same(gpu.vpod_id)", nodes=4, gpus=12)
        sim.pending(job)
        sim.cancel(job)
        sim.call("hide-fabric")
        sim.wait(lambda: all(g["status"] is None for gs in sim.inventory().values() for g in gs))
        job = sim.submit("gpu.vram>=192GiB&same(gpu.fabric_domain_id)", nodes=2, gpus=6,
                         extra=["--topology", "block"])
        assert set(sim.selected(job)) == {"sim-node-0", "sim-node-1"}
        sim.cancel(job)
        job = sim.submit("gpu.vpod_id=15", gpus=1)
        sim.pending(job)
        sim.cancel(job)
    finally:
        sim.call("reset")
        sim.wait(lambda: sim.inventory() == before)
        sim.call("block-file", "disable")


@pytest.mark.parametrize("field", ["type", "vram", "physical_pod_id"])
def test_same_global_property_groups_actual_devices(sim, field):
    count = 8 if field == "vram" else 12
    job = sim.submit(f"same(gpu.{field})", nodes=4, gpus=count)
    inventory = sim.inventory()
    selected = sim.selected(job)
    key = {"type": "type", "vram": "vram_bytes", "physical_pod_id": "physical_pod"}[field]
    values = {g[key] for node, ids in selected.items() for g in inventory[node] if g["stable_id"] in ids}
    assert len(values) == 1
    assert sum(map(len, selected.values())) == count


def test_property_selection_respects_excluded_nodes(sim):
    job = sim.submit("gpu.vram>=192GiB&same(gpu.vpod_id)", nodes=2, gpus=6,
                     extra=["--exclude", "sim-node-0", "--topology", "block"])
    assert set(sim.selected(job)) == {"sim-node-2", "sim-node-3"}


def test_exclusive_node_requires_every_reserved_gpu_to_match(sim):
    job = sim.submit("gpu.vram>=192GiB", extra=["--exclusive"])
    sim.pending(job)


def test_named_block_hostlist_expansion_is_not_implemented(sim):
    result = sim.cli("submit", "--parsable", "--gpus", 1, "--nodelist", "block{a}",
                     "--constraint", "gpu.vram>=192GiB", "--wrap", "true", check=False)
    if result.returncode == 0:
        job = int(result.stdout.strip())
        sim.jobs.append(job)
        sim.pending(job)


def test_gpu_properties_and_domain_pins_respect_named_reservations(sim):
    name = "gpu-property-reservation"
    sim.call("admin", "control", "create", f"ReservationName={name}",
             "Nodes=sim-node-[0-1]", "StartTime=now", "Duration=00:10:00", "Users=sim")
    try:
        ordinary = sim.submit("gpu.vram>=192GiB&same(gpu.vpod_id)", nodes=2, gpus=6)
        assert set(sim.selected(ordinary)) == {"sim-node-2", "sim-node-3"}
        pinned = sim.submit("gpu.vpod_id=15", nodes=2, gpus=6)
        sim.pending(pinned)
        reserved = sim.submit("gpu.vram>=192GiB&same(gpu.vpod_id)", nodes=2, gpus=6,
                              extra=["--reservation", name, "--topology", "block"])
        assert set(sim.selected(reserved)) == {"sim-node-0", "sim-node-1"}
    finally:
        for job in list(sim.jobs):
            sim.cancel(job)
        sim.call("admin", "control", "delete", f"ReservationName={name}")
