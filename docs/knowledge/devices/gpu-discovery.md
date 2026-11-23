# GPU Discovery and Device Management

Operator-facing config (`[devices]`, `[[devices.gres]]`, `cdi_spec_dirs`,
partition-mode changes, the device filter allow-list) is documented in
`docs/admin-guide/configuration.rst` and `docs/admin-guide/fabric-observation.rst`.
This page covers the cross-crate contracts behind it.

## 1. Flow

1. spurd discovers devices (CDI specs, KFD auto-detection, GRES config) into a
   `DeviceRegistry` (`spur-devices`) and reports the GPUs to the controller as
   `Device` entries in its `ResourceSet.devices`, keyed by `stable_id`.
2. The controller picks GPUs by `stable_id` (`build_node_allocation`,
   `build_exclusive_allocation` in `spur-core/src/resource.rs`) and stamps the
   allocation with the inventory `generation`.
3. spurd records the allocation in its per-node ledger (`NodeAllocation`, see
   [scheduling.md](../scheduler/scheduling.md#agent-side-allocation-ledger)),
   resolves the stable_ids through `DeviceRegistry::resolve_by_ids`, and builds
   host and container injection plans.
4. The plans feed the job's GPU env vars, the container device/mount setup, and
   the BPF device cgroup filter (see
   [job-execution.md](../execution/job-execution.md#6-device-isolation)).

## 2. Discovery

- `CdiCache::load` reads `.json`/`.yaml`/`.yml` specs from `/etc/cdi`,
  `/var/run/cdi`, then `cdi_spec_dirs`. A later directory wins on a duplicate
  qualified name (`/var/run/cdi` beats `/etc/cdi`). Specs outside
  `MIN_CDI_VERSION`..`MAX_CDI_VERSION` (`cdi/spec.rs`) are rejected.
- With `auto_detect`, live discovery fills every gres name that has no on-disk
  spec (`CdiCache::merge_live`); a gres name with an on-disk spec only gets a
  topology refresh, and is reported through `skipped_discovery()`. Authority is
  per gres name (`kind_to_gres_name`), so `amd.com/gpu` and `hpc.example.org/gpu`
  are one kind. GPU discovery (`discover_gpu_cdi`, `discovery/amd_gpu.rs`) is
  AMD-only: `is_amd_gpu_node` requires the AMD vendor id, `simd_count > 0`, and
  `cpu_cores_count == 0`, and nodes with `drm_render_minor == 0` are skipped.
- A discovered GPU carries its `renderD*` node plus the `card*` node that
  `/sys/class/drm` links to it. With no such link it carries the render node
  alone: a guessed card index can name another GPU's card.
- The vendor comes from the CDI kind prefix (`vendor_from_kind`, `amd.com/gpu` ->
  `amd`). For GRES entries without an explicit env flag it is inferred from the
  device paths (`/dev/dri/` -> AMD, `/dev/nvidia` -> NVIDIA). The vendor selects
  the visibility env var (`gpu_visibility_for_vendor`).
- GRES entries with a file spec are injectable (per-device nodes and env);
  `count_only` or file-less entries are countable pools tracked by quantity only.

## 3. Registry population and merge

`DeviceRegistry::populate` builds the registry in this order:

1. CDI entries (`sort_cdi_entries`): those with a PCI address by address, then
   `stable_id`, then primary device path; then those without one by primary
   device path; the qualified name breaks any remaining tie (partitions share a
   BDF, so the tie-breaks keep the order stable across a refresh). CDI
   aggregates (a device whose nodes are the union of two or more siblings, like
   `all`) are dropped, since they would make each node name several devices.
2. Each GRES injectable entry is merged into the discovered entries of its gres
   name (`merge_into_discovered`, targets from `merge_targets`) instead of
   becoming a second device. Only its own nodes count (`own_node_paths`, not
   `/dev/kfd`): a node only one discovered entry has names that entry; when none
   does, a shared node (a partitioned GPU's `cardN`) names every entry behind
   it. The configured `type`/`cores` win, device nodes stay
   discovery's, and its other own nodes are dropped with a warning. An entry
   naming only nodes every device of its kind shares is dropped with a warning.
3. Unmerged GRES injectable entries, then GRES countable pools.
4. `dedup_by_device_paths` drops whatever did not merge and duplicates an
   earlier entry: CDI against CDI by identical node set, a GRES entry against
   anything that owns one of its own nodes (CDI wins).
5. `assign_device_ids` assigns a sequential `device_id` per `gres_name` and
   backfills `stable_id = device_id` where it is 0 (annotation-less CDI, GRES),
   so every injectable device has a distinct id. Nonzero stable_ids are never
   overwritten.

## 4. Stable IDs vs positional device IDs

DRM render minors can shift after a driver reload, so Spur carries two ids:

- `device_id`: zero-based positional index per `gres_name`. Used for runtime
  visibility vars (`ROCR_VISIBLE_DEVICES=0,1`), which expect contiguous indices.
- `stable_id`: derived from the PCIe BDF plus a partition rank
  (`encode_stable_id` in `pci.rs`):

```text
bits 24..39 = domain & 0xffff
bits 16..23 = bus
bits 11..15 = device
bits  8..10 = function
bits  0..7  = partition_rank (STABLE_ID_PARTITION_MASK = 0xFF)
```

- The layout is consumed outside Rust by the example hook scripts; see
  `crates/spur-devices/AGENTS.md` (Coupling).
- Two stable_ids are the same physical GPU iff `id & !STABLE_ID_PARTITION_MASK`
  is equal. A compute-partitioned GPU (e.g. MI300X in CPX mode) exposes several KFD
  nodes on one BDF; each partition gets its own stable_id and is scheduled as an
  independent GPU.
- `resolve_pci_identities`: the low 3 bits of `location_id` are the PCI function
  when a (domain, bus, device) slot has one KFD node, and the partition rank when
  it has several.
- Allocation keys on `stable_id`, never `device_id`: `ResourceSet::available_device_ids`,
  `NodeAllocation`, and `DeviceRegistry::resolve_by_ids` all use it, and
  `AllocatedDevice::device_id` carries the stable_id. A render-minor shift or
  driver reload therefore does not corrupt in-flight allocations.

## 5. Repartition and renumbering safety

- `ResourceSet::generation` is bumped by the agent on every topology rebuild and
  stamped into each allocation. The agent rejects a dispatch whose generation no
  longer matches its live inventory, so a repartition between schedule and launch
  is caught even if it reuses a stable_id value for a different logical device.
- If an allocation holds a GPU stable_id missing from the current inventory (id 0
  included), `ResourceSet::holds` returns `None` and the node offers zero GPUs
  until that job finishes: the held device may now be reported under another id,
  so no GPU can be trusted free.
- `DeviceRegistry::with_pinned`: when a topology rebuild reuses a held stable_id
  for a different device, the pinned id keeps its previous `DeviceEntry` (old
  render node path) instead of the new entry sharing the id.

## 6. Injection plans

- `HostInjectionPlan.visible_devices` keeps only paths that exist on the host;
  `device_paths` (per-device plus shared) is what the BPF filter allows.
- `ContainerInjectionPlan` device nodes fall back to the job's uid/gid when the
  CDI spec omits them. Mounts are sorted by path depth (shallowest first).
- `DeviceRegistry::build_job_injection_plans` takes a `DeviceSelection` (gres
  name to stable ids) and builds one plan across every allocated kind.
- Shared edits (e.g. `/dev/kfd`, ROCm lib mounts, `video`/`render` GIDs) are
  applied once per distinct set (`merge_edits_from_devices`), so two specs of
  one kind both contribute.
- Device nodes dedup by container path and mounts by container target, first
  occurrence wins, per-device before shared. A `/dev/kfd` in both yields one entry.

## 7. GPU environment variables

User-visible semantics are in `docs/user-guide/interactive.rst`.

- The injection plan sets the vendor visibility var (`ROCR_VISIBLE_DEVICES`,
  `CUDA_VISIBLE_DEVICES`, `ZE_AFFINITY_MASK`, or `GPU_DEVICE_ORDINAL` with the
  `opencl_env` flag) and `SPUR_JOB_GPUS` (`apply_spur_job_device_env`) to the
  positional `device_id` list. Only `DeviceClass::Gpu` entries (gres `gpu`) carry
  visibility vars, so a NIC of vendor `amd` never sets `ROCR_VISIBLE_DEVICES`.
- `apply_gpu_bind_env` (`spur-core/src/task_launch.rs`) runs only when
  `SPUR_GPU_BIND`/`SLURM_GPU_BIND` is set. It writes the allocated list (or the
  `map_gpu`/`mask_gpu` override) to `SPUR_JOB_GPUS`, `ROCR_VISIBLE_DEVICES`,
  `CUDA_VISIBLE_DEVICES`, and `GPU_DEVICE_ORDINAL`, and `HIP_VISIBLE_DEVICES` as
  `0..n-1`.
- Why HIP is renumbered: `ROCR_VISIBLE_DEVICES` filters and renumbers the
  driver-visible set, then HIP applies `HIP_VISIBLE_DEVICES` to what survived.
  Absolute indices there name devices that no longer exist ("no ROCm-capable
  device is detected"). `HIP_VISIBLE_DEVICES` outranks `CUDA_VISIBLE_DEVICES` on
  ROCm, so the CUDA var stays absolute for NVIDIA, where nothing narrowed the set.
- Zero-GPU denial (`gpu_deny_visibility`): unset is not reliably "no devices", so
  a job with no GPUs gets explicit tokens: `-1` for `ROCR_VISIBLE_DEVICES`,
  `HIP_VISIBLE_DEVICES`, `CUDA_VISIBLE_DEVICES`, `GPU_DEVICE_ORDINAL`,
  `ZE_AFFINITY_MASK`; `void` for `NVIDIA_VISIBLE_DEVICES`
  (nvidia-container-runtime's deny token); empty `SPUR_JOB_GPUS`. Zero allocated
  GPUs deny regardless of bind mode, so `map_gpu`/`mask_gpu` cannot name a device
  the job was not granted.
- Per-task slicing (`PER_TASK_GPU_VISIBILITY_BASH`) splits `SPUR_LOCAL_GPUS`
  (set only inside a GPU-fenced step) or else `SPUR_JOB_GPUS` evenly across the
  node's tasks and exports each slice as `ROCR_VISIBLE_DEVICES`,
  `CUDA_VISIBLE_DEVICES`, `GPU_DEVICE_ORDINAL`, with `HIP_VISIBLE_DEVICES`
  renumbered. `ZE_AFFINITY_MASK` is not split. The slice is env-only: all tasks
  share the job's cgroup and device filter.
- Inside a fence, `localize_gpu_selectors` renumbers the runtime selectors
  (including `ZE_AFFINITY_MASK`); user-visible behavior is in
  `docs/user-guide/interactive.rst`.

## 8. Annotations

CDI device annotations use the `spur.amd.com/` prefix; the keys are the
constants in `cdi/annotations.rs` (parsed into `DeviceMetadata`). A missing
`stable-id` triggers the backfill in section 3, and `auto-detected` marks a spec
Spur owns and may rewrite (see
[containers-k0s.md](../execution/containers-k0s.md#gpu-cdi-spec-and-storage-for-k0s-pods)).