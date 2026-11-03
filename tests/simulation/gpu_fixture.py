# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Generate synthetic driver files for isolated, hardware-free Spur E2E tests."""

import argparse
import hashlib
import json
import os
from pathlib import Path

POD = "00000000-0000-4000-8000-000000000001"


def materialize(root: Path, node: int, *, grouped=False, nodes=4):
    """Write only below a caller-created disposable sysfs directory."""
    if nodes < 2 or nodes % 2 or not 0 <= node < nodes:
        raise ValueError("use an even node count and an index inside that count")
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        raise ValueError("fixture destination must be empty")
    expected = []

    def put(path, value):
        target = root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(str(value) + "\n")
        target.chmod(0o444)

    def link(path, target):
        path = root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.symlink_to(os.path.relpath(root / target, path.parent))

    half = nodes // 2
    vpod = 15 if node < half else 16
    first = 0 if node < half else half * 4
    active = list(range(first, first + half * 4))
    for gpu in range(4):
        domain = gpu // 2 + 1 if grouped else gpu + 1
        bus = 4 + gpu % 2 if grouped else 4
        bdf = f"{domain:04x}:{bus:02x}:00.0"
        bridge = f"pci{domain:04x}:00"
        pci = f"devices/{bridge}/{domain:04x}:00:08.1/{bdf}"
        kfd = f"class/kfd/kfd/topology/nodes/{gpu + 2}"
        minor = 128 + gpu * 8
        numa = gpu // 2 if grouped else -1
        vram = [24, 192, 192, 288][gpu] * 1024**3 if grouped else 288 * 1024**3
        device_id = 0x74A1 if grouped else 0x75C7
        stable_id = (domain << 24) | (bus << 16)
        properties = {
            "cpu_cores_count": 0, "simd_count": 256, "vendor_id": 0x1002,
            "device_id": device_id, "location_id": bus << 8, "domain": domain,
            "drm_render_minor": minor, "hive_id": 1, "unique_id": 4097 + gpu,
            "num_xcc": 8, "gfx_target_version": 95000,
        }
        put(f"{kfd}/properties", "\n".join(f"{k} {v}" for k, v in properties.items()))
        put(f"{kfd}/gpu_id", gpu + 1)
        put(f"{kfd}/mem_banks/0/properties", f"heap_type 1\nsize_in_bytes {vram}")
        for peer in range(4):
            if peer != gpu:
                put(f"{kfd}/io_links/{peer}/properties",
                    f"type 2\nnode_from {gpu + 2}\nnode_to {peer + 2}\nweight 10\nmax_bandwidth 100000")
        for name, value in {
            "vendor": "0x1002", "device": hex(device_id), "numa_node": numa,
            "current_compute_partition": "SPX", "current_memory_partition": "NPS1",
            "unique_id": f"{4097 + gpu:016x}", "mem_info_vram_total": vram,
        }.items():
            put(f"{pci}/{name}", value)
        for name, value in {
            "ppod_id": POD, "ppod_size": max(72, nodes * 4), "vpod_id": vpod,
            "vpod_size": half * 4, "vpod_active_accels": " ".join(map(str, active)),
            "local_accels": " ".join(str(node * 4 + g) for g in range(4)),
            "accel_id": node * 4 + gpu, "accel_state": "active", "link_type": "UALoE",
        }.items():
            put(f"{pci}/ualink/{name}", value)
        (root / pci / "drm" / f"renderD{minor}").mkdir(parents=True)
        link(f"bus/pci/devices/{bdf}", pci)
        link(f"class/drm/renderD{minor}/device", pci)
        link(f"class/drm/card{gpu}/device", pci)
        expected.append({"stable_id": stable_id, "bdf": bdf, "pci_root": bridge,
                         "numa": None if numa < 0 else numa, "vram_bytes": vram,
                         "type": "mi300x" if grouped else "amdgpu-0x75c7",
                         "physical_pod": POD, "vpod": str(vpod), "endpoint": str(node * 4 + gpu)})
    (root / "fs/cgroup").mkdir(parents=True)
    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            files[str(path.relative_to(root))] = {"symlink": os.readlink(path)}
        elif path.is_file():
            files[str(path.relative_to(root))] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
    return {"synthetic": True, "node": node, "nodes": nodes, "grouped": grouped,
            "gpus": expected, "files": files}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--node", type=int, required=True)
    parser.add_argument("--nodes", type=int, default=4)
    parser.add_argument("--grouped", action="store_true")
    args = parser.parse_args()
    print(json.dumps(materialize(args.root, args.node, grouped=args.grouped, nodes=args.nodes), indent=2))
