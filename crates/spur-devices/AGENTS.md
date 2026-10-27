# AGENTS.md -- spur-devices

Vendor-agnostic device discovery, caching, and injection for Spur.
Handles GPU (and other GRES) detection, registration, and container/host
injection planning. No runtime dependencies beyond serde, semver, tracing,
anyhow.

## Modules

```
src/
  lib.rs              -- public re-exports
  types.rs            -- LinkType enum (Pcie, Xgmi, Nvlink)
  inject.rs           -- injection plan builders (host + container)
  cdi/
    spec.rs           -- CDI spec data model (CdiSpec, CdiDevice, ContainerEdits, DeviceNode, Mount, Hook)
    cache.rs          -- CdiCache: load specs from /etc/cdi, /var/run/cdi, or in-memory
    discovery.rs      -- KFD-based AMD GPU auto-detection -> CdiSpec
    annotations.rs    -- spur.amd.com/* annotation keys + DeviceMetadata struct
  gres/
    entry.rs          -- GresEntry (Slurm-style resource config), hostlist expansion, flag-driven env injection
    cache.rs          -- GresCache: expand GRES entries into injectable devices or countable pools
  registry/
    entry.rs          -- DeviceEntry, DeviceCapability (Injectable | CountablePool), CDI-to-entry conversion
    device_registry.rs -- DeviceRegistry: unified device inventory, dedup, ID assignment, injection plan building
```

## CDI spec handling

CDI (Container Device Interface) specs define how devices get exposed to
containers. The crate supports CDI versions 0.3.0 through 1.1.0.

`CdiSpec` (spec.rs) is the root object: `kind` (e.g. `amd.com/gpu`),
`devices` list, optional `containerEdits` (shared across all devices).
Each `CdiDevice` has per-device `containerEdits`. Both serialize to/from
JSON and YAML.

Qualified names follow `vendor.domain/class=device` format
(e.g. `amd.com/gpu=0`). Parsed via `parse_qualified_name()`.

`ContainerEdits` carries: device nodes, mounts, env vars, hooks,
additional GIDs, Intel RDT, net devices (v1.1.0+).

## CDI cache

`CdiCache` (cache.rs) loads and indexes CDI specs from directories.
Default search paths: `/etc/cdi`, `/var/run/cdi`, plus any extras.
Accepts `.json`, `.yaml`, `.yml` files. Later directories have higher
priority (same qualified name -> later dir wins).

`CdiCache::load()` optionally runs auto-detection when no on-disk specs
exist and `auto_detect` is true.

## GPU auto-detection (discovery.rs)

Reads `/sys/class/kfd/kfd/topology/nodes` to enumerate AMD GPUs. Filters
by vendor_id 0x1002, simd_count > 0, cpu_cores == 0. Extracts:

- render minor number, device ID, PCI BDF (from location_id + domain)
- VRAM size (from mem_banks heap_type=1)
- IO link topology (XGMI detection from link_type=2)
- Compute/memory partition, unique ID, numa node

Produces `CdiSpec` with `amd.com/gpu` kind. Each GPU gets two device
nodes (renderD* and card*) with stat-derived major/minor/mode/uid/gid.
Shared edits include `/dev/kfd`, ROCm lib mounts, video/render GIDs.

**stable_id**: BDF-derived reload-invariant device identifier. Encodes
domain:bus:dev.func plus intra-BDF partition rank. Low 8 bits =
partition index (STABLE_ID_PARTITION_MASK = 0xFF). Survives driver
reloads that shift render minor numbers.

GPU type detection: maps PCI device IDs to known SKUs (mi300x, mi350x,
mi355x, mi210, etc.). Falls back to sysfs product_name normalization.

## GRES system (gres/)

Slurm-compatible Generic RESource definitions. `GresEntry` specifies
a resource by name, optional type, device file pattern, count, core
affinity, link weights, and flags.

**File expansion**: `expand_hostlist("/dev/dri/renderD[128-130]")` ->
three paths. `expand_multiple_files()` handles comma-separated paths
or bracket notation.

**Flags**: `amd_gpu_env` (sets ROCR_VISIBLE_DEVICES), `nvidia_gpu_env`
(CUDA_VISIBLE_DEVICES), `intel_gpu_env` (ZE_AFFINITY_MASK), `opencl_env`
(GPU_DEVICE_ORDINAL), `count_only` (no injection), `no_gpu_env`
(suppress all). Without flags, vendor is inferred from device paths
(`/dev/dri/` -> AMD, `/dev/nvidia` -> NVIDIA).

`GresCache` splits entries into injectable devices (with file specs)
and countable pools (count-only, no device paths).

## Device registry

`DeviceRegistry` (device_registry.rs) is the unified device inventory.
`populate()` merges CDI cache + GRES cache:

1. CDI entries first (sorted by PCI BDF, then by primary device path)
2. GRES injectable entries
3. GRES countable pools
4. Dedup by exact device-node path match (CDI wins)
5. Assign sequential device_ids per gres_name
6. Backfill stable_id from device_id when annotation absent

`DeviceEntry` (entry.rs) carries all scheduling-relevant fields:
gres_name, kind, device_id, stable_id, capability (Injectable vs
CountablePool), memory_mb, numa_node, cores, links, link_type, pci_bdf,
vendor, inject_actions, device/shared edits.

`resolve_by_ids()` looks up devices by stable_id (not positional
device_id). Returns error if any ID is missing.

## Device injection (inject.rs)

Two injectors: `HostInjector` and `ContainerInjector`. Both consume
`MergedEdits` (device-specific + shared edits, deduplicated).

`merge_edits_from_devices()`: aggregates per-device edits; applies
shared edits once per kind.

**ContainerInjectionPlan**: device nodes (with uid/gid fallback to job
owner), bind mounts (sorted by depth), env vars, hooks, additional GIDs.
Device nodes and mounts are deduplicated (first wins, per-device before
shared).

**HostInjectionPlan**: env vars, visible device paths (filtered to
existing), all device paths.

**GPU visibility**: sets vendor-appropriate env var
(ROCR_VISIBLE_DEVICES / CUDA_VISIBLE_DEVICES / ZE_AFFINITY_MASK) to
the comma-separated device_id list. Empty device list -> empty string
(hides all GPUs).

`SPUR_JOB_GPUS` env var is always set when GPU gres devices are present.

## Annotations (cdi/annotations.rs)

All annotations use `spur.amd.com/` prefix. `DeviceMetadata` struct
round-trips through annotation maps. Key fields: gpu-type, memory-mb,
numa-node, cores (range notation: "0-3,8,16-18"), links (weight matrix),
link-type, pci-bdf, compute-partition, memory-partition, unique-id,
render-minor, stable-id, auto-detected.

## Link type resolution

`resolve_link_type()` (entry.rs): explicit annotation wins. Otherwise
infers from link weights + vendor (AMD -> XGMI, NVIDIA -> NVLink,
unknown -> PCIe). Default is PCIe when no link data present.

## Testing

```bash
cargo test -p spur-devices
```

Uses tempfile for filesystem-based CDI spec tests. KFD topology tests
use synthetic sysfs fixtures. No network or hardware required.
