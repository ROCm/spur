# GPU Discovery and Device Management

Source: `spur-devices/src/` (CDI, GRES, registry, inject), `spur-core/src/resource.rs`,
`spur-core/src/task_launch.rs`, `spurd/src/device_cgroup.rs`, `spurd/src/executor.rs`,
`spur-sched/src/cons_tres.rs`.

---

## 1. Discovery Methods

Spur discovers GPUs through two independent mechanisms. Both feed the unified
`DeviceRegistry`, which deduplicates and assigns identifiers.

### 1.1 CDI (Container Device Interface) Discovery

CDI is the primary discovery path. It supports CDI spec versions 0.3.0 through
1.1.0.

**On-disk specs.** `CdiCache::load()` reads `.json`, `.yaml`, and `.yml` files
from `/etc/cdi`, `/var/run/cdi`, plus any operator-configured extra directories.
Later directories have higher priority -- a qualified name that appears in both
`/etc/cdi` and `/var/run/cdi` takes the `/var/run/cdi` version.

**Auto-detection.** When no on-disk specs exist and `auto_detect` is enabled,
`discover_to_cdi()` enumerates AMD GPUs from the KFD topology
(`/sys/class/kfd/kfd/topology/nodes`). A KFD node qualifies as an AMD GPU when:

- `vendor_id == 0x1002`
- `simd_count > 0`
- `cpu_cores_count == 0`
- `drm_render_minor != 0`

For each qualifying node the discovery reads:

| Attribute | Source |
|---|---|
| Render minor | `drm_render_minor` in node properties |
| PCI BDF | Decoded from `location_id` and `domain` |
| VRAM | `mem_banks/*/properties` where `heap_type == 1` |
| XGMI links | `io_links/*/properties` where `type == 2` |
| GPU type | PCI `device_id` lookup (0x74a1 = mi300x, 0x75a0 = mi350x, etc.) or `product_name` normalization |
| NUMA node | `/sys/bus/pci/devices/<bdf>/numa_node` |
| Compute/memory partition | `current_compute_partition`, `current_memory_partition` sysfs attributes |
| Unique ID | `unique_id` from properties, or sysfs `unique_id` file |

The result is a `CdiSpec` with kind `amd.com/gpu`. Each device gets two device
nodes (renderD\* and card\*) with stat-derived major/minor/mode/uid/gid. Shared
edits include `/dev/kfd`, ROCm library bind-mounts (`/opt/rocm/lib`,
`/opt/rocm/lib64`), and `video`/`render` supplementary GIDs.

**CDI qualified names** follow the `vendor.domain/class=device` format
(e.g. `amd.com/gpu=0`). The vendor label is extracted from the kind prefix
(`amd.com/gpu` -> `amd`).

### 1.2 GRES (Generic RESource) Discovery

GRES provides a Slurm-compatible config-driven discovery path. A `GresEntry`
specifies a resource by name, optional type, device file pattern, count, core
affinity, link weights, and flags.

```yaml
# Example GRES entries
- name: gpu
  type: mi300x
  file: "/dev/dri/renderD[128-135]"
  flags: [amd_gpu_env]

- name: bandwidth
  type: lustre
  count: 4096
  flags: [count_only]
```

**File expansion.** `expand_hostlist("/dev/dri/renderD[128-130]")` expands to
three paths. `expand_multiple_files()` handles comma-separated paths or bracket
notation, producing a single device with multiple nodes (e.g. renderD128 +
card0 + kfd as one logical device).

GRES entries split into two categories:

- **Injectable** -- have file specs and produce per-device entries with device
  nodes, mounts, and env injection actions.
- **Countable pools** -- `count_only` flag or no file spec. Scheduler tracks
  quantity only; no device nodes are injected.

### 1.3 Registry Population Order

`DeviceRegistry::populate()` merges both sources in a fixed order:

1. CDI entries (sorted by PCI BDF, then by stable_id, then by primary device path)
2. GRES injectable entries
3. GRES countable pools
4. Dedup by exact device-node path match (CDI wins over GRES)
5. Assign sequential `device_id` per `gres_name`
6. Backfill `stable_id` from `device_id` when the annotation is absent (0)

When CDI auto-detection is on and GRES also lists the same GPUs, CDI wins the
dedup. This is a config mistake the system does not attempt to fix -- the
warning is logged and the duplicate is dropped.

---

## 2. Stable IDs vs DRM Device IDs

The Linux DRM subsystem assigns render minor numbers (128, 129, ...) that can
shift after a driver reload. Spur uses two layers of identity:

**`device_id` (positional).** A zero-based sequential index per `gres_name`,
assigned by the registry. Used in env vars like `ROCR_VISIBLE_DEVICES=0,1`
because the GPU runtime expects these contiguous indices.

**`stable_id` (reload-invariant).** Derived from the PCIe BDF
(domain:bus:dev.func) plus a per-partition rank. The encoding:

```text
bits 24..39 = domain & 0xffff
bits 16..23 = bus
bits 11..15 = device
bits  8..10 = function
bits  0..7  = partition_rank (STABLE_ID_PARTITION_MASK = 0xFF)
```

Two stable_ids share the same physical GPU if their BDF anchors
(`id & !STABLE_ID_PARTITION_MASK`) are equal. This matters for
compute-partitioned GPUs (e.g. MI300X in CPX mode produces 8 KFD nodes on
one BDF).

**Partition rank resolution.** When multiple KFD nodes share the same
(domain, bus, device) slot, the low 3 bits of `location_id` encode the
partition rank (not the PCI function). A lone node on a slot keeps those bits
as the PCI function. This is determined by counting how many KFD nodes map to
each slot.

**Allocation keys on stable_id.** The scheduler (`cons_tres.rs`),
`ResourceSet::available_device_ids()`, and `DeviceRegistry::resolve_by_ids()`
all key on `stable_id`, not positional `device_id`. This ensures a driver
reload that shifts render minors does not corrupt in-flight allocations.

**Backfill for legacy/annotation-less devices.** When a CDI spec lacks a
`spur.amd.com/stable-id` annotation, `stable_id` defaults to 0. The registry
backfills it from `device_id` so every injectable entry has a distinct identity.
Real nonzero stable_ids from KFD discovery are never overwritten.

---

## 3. GPU Allocation in the Scheduler

### 3.1 Resource Model

`ResourceSet` (resource.rs) carries the node's inventory:

- `cpus`, `memory_mb` -- scalar resources
- `gpus: Vec<GpuResource>` -- per-GPU: device_id, gpu_type, memory_mb,
  peer_gpus, link_type, stable_id, topology
- `generic: HashMap<String, u64>` -- countable GRES (bandwidth, licenses)
- `generation` -- bumped on every topology rebuild

`ResourceAllocations` tracks consumed resources. GPU allocations are stored as
`AllocatedDevice { device_id: u64, count: u64 }` where `device_id` carries the
GPU's `stable_id`.

### 3.2 cons_tres: Consumable TRES Tracking

`NodeAllocation` (cons_tres.rs) tracks per-core, per-GPU allocation bitmaps
within a node. For GPUs:

- `gpu_allocated: Vec<bool>` -- one bool per GPU in the inventory
- `gpus: Vec<GpuResource>` -- the node's GPU table

Key operations:

- **`reserve_job()`** -- marks GPU stable_ids as allocated. Detects collisions
  (already-allocated or fenced GPUs) and returns typed errors (`GpuInUse`,
  `GpuNotFound`).
- **`release_job()`** -- clears GPU allocation bits by stable_id.
- **`free_gpus()`** -- counts unallocated GPUs, optionally filtered by type.
- **`refresh()`** -- rebuilds capacity from a fresh inventory. Refuses if any
  held GPU disappeared (its stable_id is missing from the new set), which
  prevents silent double-booking after a repartition.

### 3.3 GRES Request Parsing

GRES strings follow Slurm syntax: `gpu:mi300x:4`, `gpu:2`, `license`. Parsed
by `parse_gres()` into `(name, optional_type, count)`.

### 3.4 Node-Level Allocation

`build_node_allocation()` picks specific GPU stable_ids from the node's
available pool:

1. Groups requested GPUs by type
2. For each type, calls `pick_devices()` which returns the first `count`
   unallocated stable_ids matching the type
3. `"any"` type matches all GPU types

`build_exclusive_allocation()` claims every CPU and GPU on the node for
exclusive jobs. It emits stable_ids, not positional device_ids.

### 3.5 Inventory Generation and Repartition Safety

Each `ResourceSet` carries a `generation` counter. An allocation records the
generation it was made under. When the agent reports a new inventory (e.g. after
a GPU repartition), the controller detects whether a held GPU's stable_id
disappeared:

- If a stable_id an allocation holds is missing from the new inventory, the
  node cannot offer any GPU until the held job finishes (the
  `holds_unknown_gpu` check).
- This prevents a repartition from silently reusing a stable_id for a different
  logical device.

---

## 4. Device Injection Plans

Once the scheduler picks GPU stable_ids for a job, the agent resolves them
through the `DeviceRegistry` and builds injection plans.

### 4.1 HostInjectionPlan (bare-metal / non-container jobs)

- **env** -- GPU visibility vars (ROCR_VISIBLE_DEVICES, etc.) plus SPUR_JOB_GPUS
- **visible_devices** -- device paths that actually exist on the host
  (filtered via `Path::exists()`)
- **device_paths** -- all device paths (per-device + shared)

### 4.2 ContainerInjectionPlan (container jobs)

- **device_nodes** -- DeviceNodePlan entries with host_path, container_path,
  major/minor, permissions. UID/GID falls back to the job owner when the CDI
  spec does not specify them.
- **mounts** -- BindMountPlan entries, sorted by path depth (shallowest first).
  ROCm lib mounts are read-only.
- **env** -- merged from CDI edits plus GPU visibility vars
- **hooks** -- OCI lifecycle hooks from CDI specs (e.g. createContainer)
- **additional_gids** -- supplementary group IDs (video, render)

### 4.3 Deduplication

Both injectors deduplicate:

- Device nodes: first occurrence per container path wins (per-device before
  shared). A `/dev/kfd` in both per-device and shared edits produces one entry.
- Mounts: first occurrence per container target path wins.
- Shared edits: applied once per kind. Two GPUs from `amd.com/gpu` share one
  set of shared edits.

---

## 5. GPU Environment Variables

### 5.1 Variables Set by the Injection Plan

The injection plan sets the vendor-appropriate visibility variable to
comma-separated positional device_ids:

| Vendor | Variable | Value |
|---|---|---|
| AMD | `ROCR_VISIBLE_DEVICES` | `0,1,2` (absolute device_ids) |
| NVIDIA | `CUDA_VISIBLE_DEVICES` | `0,1,2` (absolute device_ids) |
| Intel | `ZE_AFFINITY_MASK` | `0,1,2` |
| OpenCL | `GPU_DEVICE_ORDINAL` | `0,1,2` |

`SPUR_JOB_GPUS` is always set when GPU GRES devices are present. Its value is
the positional device_id list. An empty string means no GPUs allocated.

### 5.2 Variables Set by Task Launch

`apply_gpu_bind_env()` (task_launch.rs) runs at step launch time and sets the
full set of vendor variables:

| Variable | Value | Notes |
|---|---|---|
| `SPUR_JOB_GPUS` | Allocated device list | Spur's own list |
| `ROCR_VISIBLE_DEVICES` | Absolute device_ids | Filters and renumbers the driver-visible set |
| `HIP_VISIBLE_DEVICES` | `0..n-1` (renumbered) | Must be relative because ROCR already narrowed the set |
| `CUDA_VISIBLE_DEVICES` | Absolute device_ids | Stays absolute for NVIDIA compatibility |
| `GPU_DEVICE_ORDINAL` | Absolute device_ids | OpenCL selector |

**Why HIP is renumbered.** `ROCR_VISIBLE_DEVICES` both filters and renumbers
the driver-visible GPU set. HIP then applies `HIP_VISIBLE_DEVICES` to what
survived. Repeating the absolute indices there names devices that no longer
exist, which ROCm reports as "no ROCm-capable device is detected".
`HIP_VISIBLE_DEVICES` outranks `CUDA_VISIBLE_DEVICES` on ROCm, so the CUDA
variable stays absolute for NVIDIA where nothing narrowed the set first.

### 5.3 Zero-GPU Denial

When a job has no GPUs allocated, `gpu_deny_visibility()` sets explicit denial
tokens rather than leaving variables unset (unset is not reliably "no devices"):

- `ROCR_VISIBLE_DEVICES`, `HIP_VISIBLE_DEVICES`, `CUDA_VISIBLE_DEVICES`,
  `GPU_DEVICE_ORDINAL`, `ZE_AFFINITY_MASK` -> `-1` (invalid index)
- `NVIDIA_VISIBLE_DEVICES` -> `void` (nvidia-container-runtime's deny token)
- `SPUR_JOB_GPUS` -> empty string

### 5.4 Per-Task GPU Slicing

For multi-task jobs, a shell wrapper divides `SPUR_JOB_GPUS` evenly across
tasks on the node. Each task gets `total_gpus / tasks_per_node` GPUs, selected
by `SPUR_LOCALID * gpus_per_task` as the offset into the allocated list.

### 5.5 GPU Bind Modes

`SPUR_GPU_BIND` (or `SLURM_GPU_BIND`) controls how GPUs are mapped:

| Mode | Behavior |
|---|---|
| `closest` | Uses the controller-assigned device list as-is |
| `none` | Uses the controller-assigned device list as-is |
| `map_gpu:2,3` | Overrides visibility to the literal device list |
| `mask_gpu:...` | Applies a bitmask to select devices |

A `map_gpu`/`mask_gpu` override cannot name a device the job was not granted --
zero allocated GPUs always deny regardless of bind mode.

---

## 6. BPF Device Cgroup Filter

cgroup v2 dropped the `devices` controller. Spur implements device isolation by
building and attaching a BPF_PROG_TYPE_CGROUP_DEVICE program in-process (no
eBPF toolchain required).

### 6.1 Default-Deny Architecture

The filter starts as default-deny. Allowed devices are:

1. **Base pseudo-devices** (systemd's `DevicePolicy=closed`):
   `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`,
   `/dev/tty`, `/dev/console`, `/dev/ptmx`, `/dev/pts/*`

2. **Host infrastructure devices**: `/dev/fuse` (for Apptainer/Singularity),
   `/dev/infiniband/*` (RDMA verbs for MPI, NCCL, RCCL over IB),
   `/dev/nvidia-caps/*` (MIG capability nodes)

3. **Operator-configured extras**: `cgroup.extra_device_paths` in the node
   config

4. **Allocated device paths**: from the job's injection plan (`device_paths`)

GPU device nodes (`/dev/nvidia<N>`, `/dev/kfd`, `/dev/dri/*`) arrive
exclusively through the allocation's CDI edits. A job with no GPU allocation
gets none.

### 6.2 BPF Program Structure

The program is assembled as raw BPF instructions:

1. Extract device type, major, minor, and access bits from `bpf_cgroup_dev_ctx`
2. For each allowed device rule, emit a check sequence: compare type, major,
   minor, and test access bits. A match jumps to the allow verdict.
3. Rules with `None` major/minor match any value (wildcard, like
   `/dev/pts/*` matching all ptys)
4. Fall-through reaches the deny verdict (return 0)
5. Allow verdict returns 1

The program is loaded via `bpf(BPF_PROG_LOAD)` and attached to the job's
cgroup directory with `bpf(BPF_PROG_ATTACH, BPF_CGROUP_DEVICE)`.

### 6.3 Timing

The filter is attached before any of the job's processes join the cgroup. Since
the kernel consults the filter at `open(2)` time, a device a process already
holds open stays readable regardless of the filter.

---

## 7. Hot-Plug and Device Renumbering

Spur's stable_id design explicitly handles the case where render minor numbers
shift (e.g. after a driver reload):

- **stable_id is BDF-derived**, not render-minor-derived. Two inventories with
  different render minors but the same PCI BDF produce the same stable_id.
- **The registry resolves by stable_id**, not by positional device_id. An
  allocation's stable_ids remain valid across a render minor pool shift.
- **Generation tracking** catches repartitions. A CPX-to-SPX mode change alters
  the number of KFD nodes per BDF, changing the set of stable_ids. The
  generation mismatch prevents the agent from launching against a stale
  allocation.
- **`with_pinned()`** on the registry handles mid-flight device renames. When a
  topology rebuild reuses a stable_id for a different logical device, pinned
  allocations keep their old DeviceEntry (with its old render node path) rather
  than picking up the new entry that happens to share the id.

A node holding an allocated stable_id that disappeared from the inventory
offers zero GPUs until the holding job finishes (`holds_unknown_gpu` returns
true). This is conservative but prevents silent corruption.

---

## 8. Multi-GPU Isolation Patterns

### 8.1 Shared Node (Multiple Jobs)

Each job gets a disjoint subset of GPUs via stable_id allocation. Isolation is
enforced at three layers:

1. **Environment** -- `ROCR_VISIBLE_DEVICES` / `CUDA_VISIBLE_DEVICES` scoped to
   the job's positional device_ids
2. **cgroup device filter** -- BPF program allows only the allocated device
   nodes; access to other GPUs' render/card nodes is denied at open(2)
3. **Scheduler accounting** -- `gpu_allocated` bitmap in `NodeAllocation`
   prevents double-booking

### 8.2 Exclusive Node

`build_exclusive_allocation()` claims every GPU on the node. All stable_ids are
included in the allocation. The device filter allows all GPU device nodes.

### 8.3 Partitioned GPUs (CPX Mode)

A single physical GPU in CPX mode presents multiple KFD nodes (e.g. 8 for
MI300X). Each partition gets a distinct stable_id (same BDF anchor, different
partition_rank in the low 8 bits). They are scheduled as independent GPU
resources.

### 8.4 Multi-Task GPU Slicing

Within a single job, the per-task GPU visibility wrapper divides the allocated
set across tasks. Task `i` sees GPUs at offset `i * (total / tasks_per_node)`.
This is enforced at the environment level only -- all tasks share the cgroup
and device filter.

---

## 9. AMD vs NVIDIA Device Paths

| Aspect | AMD | NVIDIA |
|---|---|---|
| Per-GPU render | `/dev/dri/renderD<N>` | `/dev/nvidia<N>` |
| Per-GPU card | `/dev/dri/card<N>` | (none) |
| Shared control | `/dev/kfd` | `/dev/nvidiactl`, `/dev/nvidia-uvm` |
| MIG capabilities | (N/A) | `/dev/nvidia-caps/*` |
| CDI kind | `amd.com/gpu` | `nvidia.com/gpu` |
| Visibility var | `ROCR_VISIBLE_DEVICES` | `CUDA_VISIBLE_DEVICES` |
| Additional var | `HIP_VISIBLE_DEVICES` (renumbered) | `NVIDIA_VISIBLE_DEVICES` |
| Deny token | `-1` (invalid index) | `void` (nvidia-container-runtime) |
| Link type | XGMI | NVLink |
| Vendor inference | Path contains `/dev/dri/` | Path contains `/dev/nvidia` |

**Vendor inference from device paths.** When GRES entries do not carry explicit
env flags, the system infers the vendor from device paths:
`/dev/dri/` -> AMD (sets `ROCR_VISIBLE_DEVICES`),
`/dev/nvidia` -> NVIDIA (sets `CUDA_VISIBLE_DEVICES`).

**CDI vendor extraction.** `vendor_from_kind("amd.com/gpu")` returns `"amd"`,
which feeds `gpu_visibility_for_vendor()` to select the correct env var.

---

## 10. Annotations

All CDI annotations use the `spur.amd.com/` prefix. Key fields stored in
`DeviceMetadata`:

| Annotation | Example | Notes |
|---|---|---|
| `spur.amd.com/gpu-type` | `mi300x` | PCI device_id lookup or product_name normalization |
| `spur.amd.com/memory-mb` | `196608` | VRAM from KFD mem_banks |
| `spur.amd.com/numa-node` | `0` | From PCI device sysfs |
| `spur.amd.com/pci-bdf` | `0000:c1:00.0` | Decoded from location_id |
| `spur.amd.com/stable-id` | `327680` | BDF-encoded reload-invariant id |
| `spur.amd.com/render-minor` | `128` | DRM render minor number |
| `spur.amd.com/link-type` | `xgmi` | `pcie`, `xgmi`, or `nvlink` |
| `spur.amd.com/links` | `-1,4,4,0` | Weight matrix (-1 = self) |
| `spur.amd.com/cores` | `0-3,8,16-18` | Core affinity range notation |
| `spur.amd.com/compute-partition` | `CPX` | Active compute partition |
| `spur.amd.com/memory-partition` | `NPS4` | Active memory partition |
| `spur.amd.com/unique-id` | `0123456789abcdef` | Hardware unique ID |
| `spur.amd.com/auto-detected` | `true` | KFD-discovered vs admin-provided |
| `spur.amd.com/topology` | (JSON) | Serialized DeviceTopology |

Link type resolution follows a priority chain: explicit annotation > inferred
from link weights + vendor (AMD with positive weights -> XGMI, NVIDIA -> NVLink)
> default PCIe.
