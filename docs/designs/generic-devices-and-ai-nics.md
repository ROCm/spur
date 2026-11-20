---
orphan: true
---

# Generic devices and AMD AI NICs: design

Spur modeled devices as GPUs only. This note records how the device model was
made generic, how AMD Pollara AI NICs ride on it, and how the old GPU-only
fields are retired without breaking a running cluster. Operator documentation
lives in the admin guide (`ai-nics.rst`, `configuration.rst`, `upgrading.rst`).

## Device model

One record describes every allocatable device, of every kind, in node
inventory, scheduling, the agent ledger and persisted state:

```rust
pub struct Device {
    pub gres_name: String,   // "gpu", "nic", "vnic", ...
    pub device_type: String, // "mi300x", "pollara400"; "" when untyped
    pub stable_id: u64,      // allocation identity, unique per gres_name on a node
    pub index: u32,          // positional index the vendor runtime numbers by
    pub topology: Option<DeviceTopology>,
    pub numa_node: Option<u32>,
    pub affinity_cpus: Vec<u32>, // from [[devices.gres]] cores=; empty = its NUMA node's CPUs
}
```

- `ResourceSet.devices` holds the inventory. A `gres` is allocated by stable id
  when it is `gpu` or the node has devices of that `gres`; otherwise it falls back
  to the countable `generic` pool.
- Demand is a `ResourceRequest` keyed by `DeviceKey { gres_name, device_type }`,
  where an untyped key matches any type.
- The agent carries a job's devices as a `DeviceSelection` (`gres` name to stable
  ids) through launch, the ledger, supervisors and recovery.
- GPU-only concepts (partitions and fences, fabric and GPU-property bindings,
  `--gpu-bind`) stay GPU-scoped and read GPUs through the generic view.

## AI NICs

- **Discovery** (`spur-devices::discovery::amd_nic`): an `ionic`-bound function
  of vendor `0x1dd8` with an RDMA device becomes `amd.com/nic` (`0x1002`) or
  `amd.com/vnic` (`0x1003`). Its `uverbs` node is found through `ibdev`, and
  `rdma_cm` is shared. Stable ids come from the PCI address.
- **Environment**: `NCCL_IB_HCA` as an exact-match list (`=ionic_0,...`),
  `PCIDEVICE_AMD_COM_NIC`/`VNIC` like the AMD network device plugin, and
  `SPUR_JOB_NICS`/`VNICS`.
- **Isolation**: the `/dev/infiniband` nodes the registry hands out by
  allocation leave the device filter's host-infrastructure set, so a job reaches
  one only through its allocation; other RDMA nodes stay open.
  `[cgroup] constrain_rdma_devices` turns this off. The list is fixed when a job
  starts and persisted with its resources, so every step of the job, and a job
  adopted after an agent restart, gets the same access; a job started before
  the list existed has none and keeps RDMA open.

## Deprecation policy

Every legacy field follows the same rule for this release:

- **Read:** internal code reads only the new field. The legacy field is read
  only when converting persisted or older data.
- **Write:** the legacy field is still written (`proto`, Raft log, snapshots,
  supervisor state), so older readers keep working and a rollback can replay
  the log.

| Legacy field | Replacement |
|---|---|
| `proto` `ResourceSet.gpus` (3), `GpuResource`, `GpuLinkType` | `ResourceSet.devices` (8), `Device` |
| `proto` `RegisterJobAllocationRequest.gpu_devices` (8) | `allocated` (9) |
| `proto` `GpuResource.numa_node` (8), `affinity_cpus` (9) | `Device.numa_node` (6), `affinity_cpus` (7) |
| CDI annotations `spur.amd.com/memory-mb`, `links`, `link-type` (written, not read) | `topology.vram_bytes`; none for links |
| `serde` `gpus` in `ResourceSet` (Raft log, snapshots, inventory baseline) | `devices` |
| `serde` `gpu_devices` in supervisor `resources` (`descriptor.json`, `launch.json`) | `devices` |
| top-level `gpu_devices` in `launch.json` written by agents before this release (read via `resources`) | `resources.devices` |
| GPU entries in `inventory-fences.json` (written in the legacy `GpuResource` shape) | `Device` |

## Removal checklist

1. **Next release:** stop writing every legacy field above, including the
   legacy shape of `inventory-fences.json`. Keep reading them. Stop writing
   `GpuResource.numa_node`/`affinity_cpus` only when no controller older than
   this release can run: those controllers place reserved cores from them.
2. **The release after:** stop reading them and drop the `serde` aliases and
   custom conversions, with these preconditions:
   - Every controller has taken a Raft snapshot on the previous release, so
     the log holds no entry that only has `gpus`: on every controller,
     `<state_dir>/raft/snapshot.json` is newer than that upgrade (as in the
     snapshot steps of `docs/deployment/upgrading.rst`).
   - No supervisor started by a release that wrote only `gpu_devices` can
     still be alive. Supervisors outlive agent upgrades, so keep reading
     supervisor `gpu_devices` until every job started before this release has
     ended.
   - Every agent and the K8s operator send `devices` (release N or later)
     before a controller stops reading `ResourceSet.gpus`; the CLI keeps its
     legacy fallback until then too.
3. Delete the frozen v1 compatibility fixtures only after step 2.
