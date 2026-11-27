# AGENTS.md -- spur-devices

Device discovery (CDI, GRES), the unified `DeviceRegistry`, and host/container
injection plans. Discovery authority, registry merge and dedup rules, card-node
sourcing, and GPU-only visibility vars are in
`docs/knowledge/devices/gpu-discovery.md`.

## Coupling

- `encode_stable_id` (`pci.rs`) bit layout is consumed outside Rust.
  Hook env exposes these ids as `SPUR_JOB_GPUS`; the exporter hooks
  (`examples/hooks/dme/common.sh`, `examples/hooks/gpu-node/common-env.sh`)
  rebuild them from KFD `domain` and `location_id` to map each to its render
  minor, so a layout change must update those scripts too.

## Pitfalls

- `DeviceRegistry::populate` merges a GRES injectable entry onto discovered
  entries (`merge_into_discovered`) instead of only deduping it. This is how
  `[[devices.gres]] cores=` sets reserved-core affinity without disabling
  auto-detection; exact rules (including dropping CDI aggregates first) are in
  the knowledge doc.

## Testing

- Tests must not read the host: use `CdiCache::load_with()` (explicit spec dirs
  and live specs) and `discover_gpu_cdi_from_root()` (synthetic sysfs root).
