# AGENTS.md -- spur-devices

Device discovery (CDI, GRES), the unified `DeviceRegistry`, and host/container
injection plans. Behavior reference: `docs/knowledge/devices/gpu-discovery.md`.

## Coupling

- `encode_stable_id` (`pci.rs`) bit layout is consumed outside Rust.
  Hook env exposes these ids as `SPUR_JOB_GPUS`; the exporter hooks
  (`examples/hooks/dme/common.sh`, `examples/hooks/gpu-node/common-env.sh`)
  rebuild them from KFD `domain` and `location_id` to map each to its render
  minor, so a layout change must update those scripts too.

## Pitfalls

- Auto-detection authority is per gres name (`kind_to_gres_name`), not per CDI
  kind: `amd.com/gpu` and `hpc.example.org/gpu` are one kind. Live specs fill
  every gres name no on-disk spec supplies; for the others they only refresh
  topology, and the skipped names surface through `skipped_discovery()`
  (carried by `DeviceRegistry`, warned by `spurd` on change).
- `DeviceRegistry::populate` does not just dedup GRES against CDI. A GRES
  injectable entry is first merged onto the discovered entries of its gres name
  (`merge_into_discovered`, targets from `merge_targets`) instead of becoming a
  second device. Only its own nodes count (`own_node_paths`, so not `/dev/kfd`):
  a node only one discovered entry has names that entry; when none does, a
  shared node (a partitioned GPU's `cardN`) names every entry behind it. Its
  configured `type`/`cores` win, device nodes stay discovery's, and its
  other own nodes are dropped with a warning. This is how
  `[[devices.gres]] cores=` sets reserved-core affinity without disabling
  auto-detection.
- CDI aggregates (a device whose nodes are the union of two or more siblings,
  like `all`) are dropped before GRES entries merge, so they never widen a merge
  or make a node name several devices.
- A GPU's `card*` node comes only from the sysfs link to its render node, never
  a guessed index (a guessed index can name another GPU's card).
- Only `DeviceClass::Gpu` entries (gres `gpu`) set visibility env vars, so a NIC
  of vendor `amd` never sets `ROCR_VISIBLE_DEVICES`.

## Testing

- Tests must not read the host: use `CdiCache::load_with()` (explicit spec dirs
  and live specs) and `discover_gpu_cdi_from_root()` (synthetic sysfs root).
