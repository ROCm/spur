# Spur for Helios: NVL72 Rack Architecture Support

This document analyzes what is needed to run Spur on GB200 NVL72-class infrastructure ("Helios"), covering what Slurm has built for this architecture, what Spur has today, and the gaps that need to be closed.

---

## Background: The NVL72 Scheduling Problem

The NVIDIA GB200 NVL72 rack contains 18 compute trays (nodes), each with 4 Blackwell GPUs, for 72 GPUs total connected via fifth-generation NVLink in a single hardware domain. The key operational fact is that the performance cliff when crossing domain boundaries is severe: intra-domain NVLink delivers ~130 TB/s aggregate bandwidth; falling back to InfiniBand or Ethernet drops to ~50 GB/s — a roughly 25x degradation. This makes rack-scale locality a hard correctness constraint for large distributed training jobs, not a best-effort optimization.

This changes what a scheduler must do. Best-effort locality hints (like Slurm's legacy `topology/tree`) are insufficient; the scheduler must enforce domain boundaries and refuse to silently scatter a job across racks.

---

## What Slurm Built for NVL72

### `topology/block` plugin (Slurm 23.11)

Co-developed by NVIDIA and SchedMD. Models each NVLink domain as a named, non-overlapping, rigid "block" of nodes. Key properties:

- A node belongs to exactly one block.
- The plugin enforces intra-block placement for jobs that fit within a block — it does not fragment across block boundaries to reduce queue time.
- Multi-block jobs receive equal-sized allocations from each block they span.

Configured via `topology.conf`:

```
TopologyPlugin=topology/block

BlockName=rack1 Nodes=node[1-18]
BlockName=rack2 Nodes=node[19-36]
BlockSizes=18,36
```

`BlockSizes` defines the base allocation unit and optional aggregated sizes (each a power-of-two multiple of the previous), enabling a hierarchy where adjacent blocks form larger scheduling units for very large jobs.

### Segment-level atomic allocation

Jobs use `--segment=N` to specify the per-block atomic allocation unit:

```
sbatch --nodes=32 --segment=16 --gres=gpu:4 job.sh
```

This allocates 16 nodes from one block and 16 from another — two segments of 16 — guaranteeing all-NVLink within each segment. Without `--segment`, Slurm defaults to the full block size (18), which causes submission failures when even one node in the block is drained.

NVIDIA/SchedMD guidance: never use segment sizes larger than 16 on an 18-node block; always use factors of 16 (1, 2, 4, 8, 16) as segment sizes for clean bin-packing across racks.

### `switch/nvidia_imex` plugin (Slurm 24.05)

IMEX (Internode Memory Exchange) is the NVIDIA driver service that enables coherent GPU memory semantics over NVLink across nodes. Without it, cross-node NVLink on GB200 does not provide what CUDA expects — collective operations and peer memory access silently fall back or fail.

Slurm's `SwitchType=switch/nvidia_imex` manages per-job IMEX channel allocation via prolog/epilog scripts:

- **Prolog**: reads `SLURM_JOB_NODELIST`, writes `/etc/nvidia-imex/nodes_config.cfg` on each job node, starts the IMEX daemon.
- **Epilog**: stops the daemon and cleans up the config.

This provides driver-level isolation: one job's GPU memory is not accessible from another job's processes, even within the same NVLink domain. A security vulnerability in channel isolation was patched in Slurm 24.05.2; earlier 24.05.x releases were exposed.

### NVLink domain metadata exposed to jobs

| Variable / Mechanism | Description |
|---|---|
| `SLURM_JOB_NODELIST` | Definitive node list; used by IMEX service to configure peer nodes |
| `SLURM_TOPOLOGY_ADDR` | Per-task switch path; behavior for `topology/block` is not well-specified |
| `TopologyParam=BlockAsNodeRank` (25.11) | Sorts MPI rank assignment by topology position so rank ranges map to segments |
| `SLURM_JOB_SEGMENT_SIZE` | Planned; not yet shipped as of the SC'25 roadmap |
| `/etc/nvidia-imex/nodes_config.cfg` | Driver-level peer list populated by prolog from `SLURM_JOB_NODELIST` |

NCCL and distributed training frameworks are expected to use `SLURM_JOB_NODELIST` plus MPI rank-to-node mapping for locality, not a dedicated NVLink domain variable. `NCCL_TOPO_FILE` or NCCL auto-detection handles the rest.

### Multi-topology per cluster (Slurm 25.05)

`topology.yaml` (supersedes `topology.conf`) allows per-partition topology assignment:

```yaml
- name: nvl72_topology
  plugin: block
  cluster_default: false

- name: ib_topology
  plugin: tree
  cluster_default: true
```

NVL72 partitions use `topology/block`; InfiniBand partitions use `topology/tree`; both run on one controller with one config file.

### Incomplete block handling (Slurm 25.05)

Formalized support for blocks with drained or failed nodes. Previously, a single drained node could make an entire block unusable or require manual `--segment` workarounds. 25.05 allows blocks to be declared with fewer than `BlockSize` nodes without breaking all scheduling for that block.

---

## What Spur Has Today

### Topology infrastructure

- **`topology/block`** — `TopologyTree::from_blocks(all_nodes, block_size)` in [spur-core/src/topology.rs](../crates/spur-core/src/topology.rs) groups nodes into equal-size blocks named `block000`, `block001`, etc. Default block size is 18.
- **`topology/tree`** — `TopologyTree::from_switches(switch_configs)` builds a hierarchy from explicit `SwitchConfig` entries.
- **`select_local_nodes(candidates, count)`** — greedy locality selection: prefers same-switch nodes, then greedily adds nearest neighbors. Used by the backfill scheduler when `job.spec.topology` is set.

### GPU resource tracking

- `GpuLinkType::NVLink` enum value throughout the device/resource stack ([spur-core/src/resource.rs](../crates/spur-core/src/resource.rs), [proto/slurm.proto](../proto/slurm.proto)).
- `peer_gpus: Vec<u32>` on `GpuResource` — populated from GRES `links` weight matrix in [spurd/src/reporter.rs](../crates/spurd/src/reporter.rs).
- `resolve_link_type()` in [spur-devices/src/registry/entry.rs](../crates/spur-devices/src/registry/entry.rs) — infers `Nvlink` for NVIDIA vendor when the links matrix is non-zero.
- GB200 recognized as an NVIDIA GPU type in the K8s path: `is_nvidia_gpu("gb200")` in [spur-k8s/src/agent.rs](../crates/spur-k8s/src/agent.rs).

### Job environment injection

- `CUDA_VISIBLE_DEVICES`, `ROCR_VISIBLE_DEVICES`, `ZE_AFFINITY_MASK` — set per task in [spur-devices/src/inject.rs](../crates/spur-devices/src/inject.rs) and [spur-core/src/task_launch.rs](../crates/spur-core/src/task_launch.rs).
- Per-task GPU splitting via `SPUR_JOB_GPUS` — task wrapper divides allocated device IDs by tasks-on-node, then exports the vendor-specific visibility variable per rank.

### GRES

- Full GRES parsing, expansion, device matching, and per-device allocation tracking ([spur-core/src/resource.rs](../crates/spur-core/src/resource.rs), [spur-sched/src/cons_tres.rs](../crates/spur-sched/src/cons_tres.rs), [spur-devices/src/gres/](../crates/spur-devices/src/gres/)).

---

## Gaps

### [1] Block boundaries are not enforced — they are a hint

**What Slurm does:** `topology/block` refuses to schedule a job across block boundaries. Cross-block placement is structurally disallowed for jobs that fit within one block.

**What Spur does:** In [spur-sched/src/backfill.rs](../crates/spur-sched/src/backfill.rs), topology-aware placement is a post-selection reordering step. The scheduler first collects enough candidate nodes from anywhere in the cluster, then calls `select_local_nodes()` to reorder the assignment by locality. If `job.spec.topology` is not set, this step is skipped entirely. There is no hard exclusion of cross-block placement.

**Impact:** A job on an NVL72 cluster could be silently scheduled across two racks, taking the 25x bandwidth penalty, with no error or warning.

### [2] No segment abstraction

**What Slurm does:** `--segment=N` defines the atomic allocation unit within a block. A job smaller than one block gets a segment of N nodes, guaranteed intra-block. This prevents full-block waste for small jobs while still enforcing NVLink locality.

**What Spur does:** No segment concept exists. The block is the atomic unit. A job requesting fewer nodes than a full block either gets placed anywhere (if topology is not enforced) or wastes the remainder of the block.

**Impact:** Either NVLink locality is broken for sub-block jobs, or rack utilization is poor because every job consumes a full block regardless of size.

### [3] No IMEX lifecycle management

**What Slurm does:** `switch/nvidia_imex` configures the NVIDIA IMEX daemon per job via prolog/epilog. Without it, cross-node NVLink memory semantics do not work on GB200.

**What Spur does:** Nothing. Spur has no prolog/epilog hook system (or equivalent job lifecycle callbacks) that could configure IMEX. `CUDA_VISIBLE_DEVICES` is set correctly, but the driver-level inter-node NVLink channel is never established.

**Impact:** Jobs on actual GB200 hardware will not be able to use cross-node NVLink. Collective operations that rely on NVLink fabric (e.g., NCCL AllReduce across nodes) will silently fall back to InfiniBand or Ethernet, or fail.

### [4] Topology is per-job opt-in, not per-partition

**What Slurm does:** Topology is a partition-level property. Every job submitted to an NVL72 partition gets block scheduling automatically, regardless of how the job was submitted.

**What Spur does:** `job.spec.topology` must be explicitly set per job. There is no partition-level topology config that applies automatically. Users who forget the flag get no locality enforcement.

**Impact:** Correct scheduling on NVL72 hardware requires every user (or every sbatch script) to explicitly request topology-aware placement. One missing flag means a broken job layout with no diagnostic.

### [5] One topology per cluster

**What Slurm does (25.05+):** Per-partition topology via `topology.yaml`. NVL72 and non-NVL72 nodes coexist on one controller with different scheduling rules per partition.

**What Spur does:** One global `TopologyConfig` in `scheduler_loop.rs` applies to the entire cluster. There is no mechanism to give different partitions different topology plugins or block configurations.

**Impact:** A cluster with both NVL72 nodes and standard InfiniBand nodes cannot correctly apply block scheduling to only the NVL72 partition. Either block scheduling applies everywhere (breaking placement for non-NVL72 nodes) or it applies nowhere.

### [6] No incomplete block support

**What Slurm does (25.05+):** Blocks can be declared with fewer than `BlockSize` nodes. A single drained node does not make the entire block unschedulable.

**What Spur does:** `from_blocks()` does a static node-count grouping at startup. There is no mechanism to mark a block as having fewer usable nodes, adjust segment sizing, or drain a block partially. If a node in a block goes down, the block either schedules jobs onto it (which then fail) or the node is marked unavailable and the block silently becomes undersized with no scheduler awareness.

**Impact:** Node failures in an NVL72 rack degrade scheduling quality in ways the scheduler cannot reason about or compensate for.

### [7] No NVLink topology metadata injected into jobs

**What Slurm does:** `SLURM_TOPOLOGY_ADDR`, planned `SLURM_JOB_SEGMENT_SIZE`, `TopologyParam=BlockAsNodeRank`, and IMEX config at `/etc/nvidia-imex/nodes_config.cfg` together give distributed training frameworks everything they need to understand their NVLink domain layout at runtime.

**What Spur does:** Sets `CUDA_VISIBLE_DEVICES` and `ROCR_VISIBLE_DEVICES` correctly. Does not inject block name, segment peers, domain ID, or any topology metadata that NCCL or other collective communication libraries could use for topology-aware routing.

**Impact:** NCCL and similar frameworks cannot auto-discover NVLink domain boundaries from Spur-injected environment. Users must configure `NCCL_TOPO_FILE` manually or accept suboptimal collective routing.

---

## AMD Scale-Up Fabric: VPODs and AFM

AMD's scale-up fabric model is architecturally different from NVIDIA's and requires a separate analysis. The relevant hardware is the IFoE (Infinity Fabric over Ethernet) scale-up network — an extension of XGMI (AMD's intra-node GPU interconnect) carried over a single-tier Ethernet switch fabric within a rack. Every GPU must be connected to all switches; there are no switch-to-switch links. The rack is the hard boundary of the scale-up domain.

### What a vPoD is

A Virtual Pod (vPoD) is a logical partition of the scale-up fabric assigned to a set of GPUs. It is the unit of fabric isolation and the unit of GPU-to-GPU reachability. A vPoD can span one or more compute nodes within a rack.

**Current constraint:** All GPUs in a vPoD must belong to the same physical tray. Multi-node vPoDs are architecturally supported and planned; partial-GPU vPoDs (spatial partitions) are not supported on the scale-up fabric — partitioned GPUs cannot access it.

**The system vPoD:** All admitted nodes land in an auto-created system vPoD, which is fully functional — not a blackhole. Any node entering the system vPoD is programmed identically to a user-created vPoD (VLAN, FDB entries, GPU dest maps, encryption keys). User-created vPoDs are how workloads get isolated from each other and from the system vPoD.

### What AFM programs when a vPoD is created

vPoD creation is a multi-step, multi-target operation driven by AFM:

1. Allocate a VLAN for the vPoD
2. Configure that VLAN on all switches; disable MAC learning; flush all FDB entries
3. Remove GPUs from their existing vPoDs: flush dest address maps on those GPUs, delete their MAC addresses from old vPoD VLANs
4. Compute forwarding tables: query topology, determine ports associated with vPoD GPUs, compute static FDB entries per switch
5. Program dest map entries on every GPU in the vPoD (AccID → MAC mapping for all peers)
6. Move switch ports to the new VLAN
7. Distribute AES-GCM-256 encryption keys to all GPU IFoE blocks in the vPoD

All forwarding is fully SDN-controlled. MAC learning is disabled globally; unknown-destination packets are dropped. Every forwarding path that exists was explicitly programmed by AFM.

**AccIDs** (Accelerator IDs) are stable logical identifiers assigned per GPU and do not change when vPoDs are created or destroyed. GPUs address peers by AccID; the GPU's IFoE mapping table translates AccID to MAC for the switch layer. The switch layer knows only MACs and VLANs.

### Scale-up vs. scale-out

| | Scale-Up (IFoE) | Scale-Out (AINICs) |
|---|---|---|
| Scope | Within a rack | Cross-rack |
| Bandwidth | Extremely high, very low latency | Orders of magnitude lower |
| Protocol | IFoE / UALP (extension of XGMI) | RDMA / RoCEv2 |
| Memory semantics | Load/store shared memory across GPUs | Message passing |
| vPoD required | Yes | No |

Cross-rack GPU communication uses AINICs over the scale-out network and requires no vPoD setup.

### Why AMD does not need a concept like IMEX

NVIDIA requires three separate mechanisms to achieve what AMD provides in a single vPoD creation:

| What is needed | NVIDIA | AMD |
|---|---|---|
| Switch routing programmed | FM + NVLSM at boot (full domain, persistent) | AFM per vPoD (only vPoD members, per-job) |
| Hardware isolation between workloads | NVLink Partitions (PKEY-enforced, admin-time, disruptive to change) | VLANs (per vPoD, dynamic, non-disruptive to other vPoDs) |
| GPU-side peer addressing | Implicit via GFA/LID assigned by NVLSM at boot | AccID→MAC dest maps programmed per GPU per vPoD |
| Per-job access control gate | IMEX channel device file + cgroup (software gate) | Not needed — VLAN isolation is hardware-enforced |
| Cross-node memory address relay | IMEX daemon (TCP/gRPC between nodes) | mTLS agent channel with cryptographic key exchange (details not fully specified in available docs) |
| Traffic encryption | Not provided by IMEX or FM | AES-GCM-256 per vPoD, distributed at creation |

IMEX exists on NVIDIA because the NVSwitch fabric is always-on: FM programs the full domain at boot, all GPUs are permanently reachable from all other GPUs in the domain. IMEX is the mechanism that gates which processes can invoke the CUDA VMM cross-node shared-memory API (`CU_MEM_HANDLE_TYPE_FABRIC`) on top of that already-routable fabric. It is an access control layer bolted on top of pre-existing connectivity.

AMD's fabric is not always-on in this sense. The system vPoD is fully functional, but it covers all admitted nodes — not a specific job's nodes. When a job needs isolation (which multi-tenant GPU training always does), the scheduler creates a user vPoD. The VLAN boundary is enforced in the switch ASIC: a GPU on VLAN 10 physically cannot send packets to a GPU on VLAN 20 regardless of what software runs on the node. There is no need for a software gate on top because hardware already enforces the boundary. IMEX has no direct AMD equivalent — and intentionally so. Its access control role (gating which processes can invoke the CUDA VMM fabric API) is made unnecessary because VLAN isolation already prevents cross-job GPU communication at the switch hardware layer; there is no fabric path to gate. Its address relay role (propagating a specific allocation's fabric address from exporter to importer across nodes) maps to the mTLS agent channel with cryptographic key exchange, though the implementation details of that mechanism are not fully specified in available documentation. The correct parallels are: NVLink Partitions (PKEYs) → VLANs for hardware-enforced workload isolation; FM+NVLSM boot-time routing → FDB entries and GPU dest maps for forwarding infrastructure.

One area where the AMD equivalent is not fully documented: whether cross-node shared memory semantics (the specific capability IMEX enables via `cuMemImportFromShareableHandle`) require a separate userspace daemon on AMD, or whether the AFM agent infrastructure handles the NPA address relay. The spec describes cryptographic key exchange over the mTLS agent channel for memory export/import security, but the implementation details of that mechanism are not specified in available documentation.

### vPoD lifecycle models for Spur

The question of when vPoDs are created and destroyed determines the isolation model, scheduling latency, and how much AFM integration Spur requires. The options are:

| Option | vPoD created by | Granularity | Jobs share vPoD? | Job-level isolation? | Dispatch latency |
|---|---|---|---|---|---|
| 1. Per-tenant cluster | Operator via AFM | Tenant / cluster | All jobs in cluster | No | Zero |
| 2. Per-partition, pre-created | Operator via AFM | Partition | All jobs in partition | No | Zero |
| 3. Per-job, dynamic | Spur via AFM | Job | No | Yes | AFM creation time |
| 4. Per-partition, dynamic | Spur via AFM | Partition | All jobs in partition | No | AFM creation time (once per partition) |
| 5. Pool, leased per job | Operator pre-creates, Spur leases | Job (via pool) | No | Yes (after rekey) | Rekeying only |
| 6. Per-user / project | Operator or Spur | User / project | All jobs of same user | No | Depends |
| 7. Per-reservation | Spur via AFM at booking time | Reservation | All jobs in reservation | No | Zero at dispatch |

**Options 1 and 2** hand vPoD management entirely to the operator. Spur is vPoD-agnostic — it just knows a partition maps to a given vPoD ID. No AFM integration required. The cost is no job-level isolation: concurrent jobs in the same vPoD share encryption keys and GPU-to-GPU reachability.

**Option 3** gives the strongest isolation — each job gets its own VLAN, encryption keys, and dest maps. AFM creation is async and involves switch FDB programming, per-GPU dest map updates (O(N) calls for N GPUs), and key distribution. This is the right default for multi-tenant Helios clusters. The dispatch latency depends on how quickly AFM can complete the sequence; this must be measured on real hardware.

**Option 4** amortizes creation cost across all jobs in a partition but shares the vPoD across concurrent jobs. Adding or removing nodes from a live vPoD triggers DF reconfiguration (drain → reprogram → resume), which is disruptive to running jobs. This makes option 4 practical only when partition node membership is static.

**Option 5** is operationally complex: pre-created vPoDs must be rekeyed between jobs (pushing new AES keys to all GPU IFoE blocks), and pool sizes must be pre-tuned to match job size distributions. Works best when job sizes are predictable and uniform.

**Option 7** shifts AFM creation cost to reservation time — hours or days before the job starts. All jobs within the reservation share the vPoD. Zero dispatch latency. Also provides an early signal if the fabric cannot be programmed for those nodes, surfacing hardware issues before the scheduled run time.

**The likely production answer** is a combination: option 3 (per-job dynamic) as the default for multi-tenant GPU training workloads where isolation is a security requirement, option 7 (per-reservation) for large runs where dispatch latency matters and the reservation is single-user. Options 1 or 2 remain valid for single-tenant deployments where operational simplicity outweighs isolation requirements.

**Transition window:** When a vPoD is destroyed, GPUs pass through an unassigned state before returning to the system vPoD or entering a new vPoD. During this window, GPUs are unreachable at the fabric level. The scheduler must account for this: a GPU finishing one job is not immediately assignable to the next. How long this window lasts in practice is hardware-dependent and must be measured.

### Choosing the right granularity: isolation boundary first

The choice between cluster, partition, reservation, and job as the FabricDomain granularity is not primarily a performance or operational decision — it is a question of where the isolation boundary needs to be drawn.

A vPoD creation programs exactly two isolation primitives: a dedicated VLAN on the switch fabric and a unique set of AES-GCM-256 encryption keys distributed to every GPU in the domain. These are the only mechanisms that prevent one workload from reaching another's GPU memory or intercepting its traffic at the fabric layer. Everything else — scheduling locality, topology hints, env var injection — is informational. The VLAN and the keys are the boundary.

This means: **every entity that shares a vPoD shares an isolation boundary.** If two jobs run in the same vPoD, they are on the same VLAN (their GPUs can reach each other at L2) and encrypt traffic with the same keys (a compromised process in one job could potentially access fabric traffic from the other). Whether that is acceptable depends entirely on the trust model between those jobs:

- **Cluster-level vPoD (option 1):** All jobs in the cluster share one isolation boundary. Acceptable only when all users fully trust each other — equivalent to a single-tenant dedicated cluster.
- **Partition-level vPoD (options 2, 4):** All jobs in a partition share one isolation boundary. Acceptable when a partition maps to a single team or project whose members mutually trust each other, but different partitions are isolated from each other.
- **Reservation-level vPoD (option 7):** All jobs within a reservation share one isolation boundary. Acceptable when a reservation is always booked by a single user or tightly-coupled workflow, and the advance booking window is used to amortize creation cost.
- **Job-level vPoD (options 3, 5):** Each job has its own isolation boundary — its own VLAN and its own encryption keys. Required for true multi-tenant security where jobs from different users may run concurrently on the same rack.

The right granularity is therefore determined by asking: "who do I need to isolate from whom?" The answer to that question selects the option; dispatch latency and operational complexity are secondary considerations that inform the implementation but should not override the isolation requirement.

---

## Modeling Fabric Resources in Spur: The FabricDomain

### Why a first-class construct is needed

A scalar `vpod_id: Option<String>` on the job record is insufficient for three reasons:

- **Multi-rack jobs require a list, not a scalar.** The rack is the hard boundary of the IFoE scale-up domain. A job spanning two racks needs one vPoD per rack, each created against a different AFM endpoint. A single ID cannot represent this.
- **The owner is not always the job.** In the pre-created and per-partition lifecycle options, the vPoD outlives any individual job. The construct must exist independently of the job record.
- **Lifecycle state must survive controller failover.** vPoD creation is async and multi-step (VLAN, FDB, dest maps, key distribution). If the Raft leader crashes mid-creation, the new leader must be able to resume or roll back. A bare ID stored on the job record carries no state machine; a Raft-persisted entity does.

### The FabricDomain abstraction

A `FabricDomain` is the complete communication domain for a job, partition, or reservation — everything the workload can reach over the high-speed fabric, spanning both scale-up (IFoE vPoDs, one per rack) and scale-out (AINICs, implicit when the domain spans more than one rack).

```
Single-rack job                         Multi-rack job
─────────────────────────               ──────────────────────────────────────────
FabricDomain                            FabricDomain
  scale_up:                               scale_up:
    ScaleUpSegment                          ScaleUpSegment
      rack: "rack1"                           rack: "rack1"
      vpod_id: "vpod-abc"                     vpod_id: "vpod-abc"
      nodes: [n1, n2, n3, n4]                 nodes: [n1, n2]
                                          ScaleUpSegment
                                            rack: "rack2"
                                            vpod_id: "vpod-def"
                                            nodes: [n5, n6]
                                        (scale-out via AINICs is implicit:
                                         all nodes have AINICs, always-on,
                                         no per-job programming needed)
```

There is no explicit `ScaleOutSegment`. AINICs are pre-configured with IPs at cluster setup time and are always reachable — every node has one. When a FabricDomain spans more than one rack, scale-out connectivity is implicit in the multi-segment structure. No AFM call is needed for it.

### Internal structure

```rust
// Raft-persisted, keyed independently of job/partition/reservation records
struct FabricDomain {
    id: FabricDomainId,               // Spur-internal UUID
    state: FabricDomainState,
    owner: FabricDomainOwner,
    scale_up: Vec<ScaleUpSegment>,    // one per rack; >1 means scale-out is in play
}

struct ScaleUpSegment {
    rack: String,
    afm_endpoint: String,             // AFM controller URL for this rack
    vpod_id: Option<String>,          // None until AFM confirms creation
    nodes: Vec<NodeId>,
    state: SegmentState,
}

enum FabricDomainState {
    Creating,    // at least one ScaleUpSegment not yet Ready
    Ready,       // all segments Ready; job may now launch
    Destroying,
    Failed(String),
}

enum SegmentState {
    Removing,    // GPUs being removed from prior vPoD (two-step transition)
    Creating,    // AFM create call in flight
    Ready,
    Destroying,
    Failed(String),
}

enum FabricDomainOwner {
    Job(JobId),
    Partition(String),
    Reservation(ReservationId),
}
```

The job, partition, or reservation record carries `fabric_domain: Option<FabricDomainId>` — a reference, not the domain itself. This allows a single domain to be owned by a partition and shared across many jobs without embedding it in any one job record.

### FabricPlugin trait

The scheduler interacts with AFM through a vendor-agnostic trait, allowing the same scheduler logic to work for AMD (AFM + vPoDs), NVIDIA (IMEX channel pool), or clusters with no fabric management:

```rust
trait FabricPlugin: Send + Sync {
    // Called after scheduling decision, before LaunchJob dispatch.
    // racks: list of (rack_id, node_ids) derived from the scheduled node set.
    async fn allocate(&self, spec: &FabricDomainSpec) -> Result<FabricDomain>;

    // Called after job/partition/reservation completes.
    async fn release(&self, domain: &FabricDomain) -> Result<()>;

    // Called at startup to validate AFM reachability.
    async fn probe(&self) -> Result<()>;
}

struct FabricDomainSpec {
    racks: Vec<(RackId, Vec<NodeId>)>,
}
```

Implementations: `AmdAfmPlugin` (calls AFM REST API per rack), `NvidiaImexPlugin` (manages integer channel pool), `NoopPlugin` (for non-fabric partitions).

### Dispatch flow

#### Single-rack job (rack1 only)

```
1. Scheduler picks nodes {n1, n2, n3, n4} — all in rack1
2. Controller creates FabricDomain record in Raft (state: Creating)
     ScaleUpSegment { rack: rack1, state: Removing }
3. Calls AFM_rack1: remove target GPUs from current vPoD
4. AFM confirms → segment state: Creating
5. Calls AFM_rack1: POST /configs/cluster/v1/vpods with nodes={n1..n4}
6. Polls AFM_rack1 until vPoD state is Ready
     (AFM programs: VLAN, FDB on all switches, dest maps on all GPUs, encryption keys)
7. Segment state: Ready → FabricDomain state: Ready
8. Controller dispatches LaunchJob to {n1, n2, n3, n4}
     Injects SPUR_VPOD_ID, SPUR_SCALE_UP_PEERS into job environment
9. Job completes → Controller calls AFM_rack1: DELETE vPoD (two-step)
     FabricDomain state: Destroying → removed from Raft
```

#### Multi-rack job (rack1 + rack2)

```
1. Scheduler picks {n1, n2} from rack1 and {n5, n6} from rack2
2. Controller creates FabricDomain in Raft (state: Creating)
     ScaleUpSegment { rack: rack1, state: Removing }
     ScaleUpSegment { rack: rack2, state: Removing }
3. Calls AFM_rack1 and AFM_rack2 IN PARALLEL: remove GPUs from current vPoDs
4. Both confirm → both segments: Creating
5. Calls AFM_rack1.create_vpod(nodes=[n1,n2]) and
   AFM_rack2.create_vpod(nodes=[n5,n6]) IN PARALLEL
6. Polls both AFM endpoints until both segments Ready
7. FabricDomain state: Ready
8. Dispatches LaunchJob to ALL nodes {n1, n2, n5, n6}
     Injects per-rack vPoD IDs and peer lists (see Environment Injection below)
9. On completion: destroys both vPoDs in parallel, FabricDomain removed from Raft
```

**Partial failure:** If step 5 succeeds on rack1 but AFM_rack2 is unreachable, the domain stays in `Creating`. The controller retries rack2's segment. If retries are exhausted, domain transitions to `Failed`: rack1's vPoD is destroyed, the job is marked failed or requeued, and GPUs return to the system vPoD on each rack. A partially-ready FabricDomain is never handed to a job.

### Partition as the fabric policy anchor

Every job in Spur is always associated with a partition — there are no partition-less jobs. When a user submits without `--partition`, `apply_default_partition()` assigns the partition marked `is_default = true` in config, falling back to the first partition if none is marked. This is enforced at submit time in `spurctld` before the job enters the scheduling queue.

This makes the partition the natural and complete anchor for fabric policy. Every job that reaches the scheduler already has a partition, so the fabric `isolation` mode on that partition is always reachable — no edge cases.

Nodes that register without matching any explicit partition hostlist are automatically assigned to the default partition, inheriting its fabric policy. This means the default partition effectively sets the cluster-wide fabric default for all unmanaged nodes.

**The default partition is where cluster-wide isolation policy is set.** On a multi-tenant Helios cluster where per-job isolation is the safe default:

```toml
[[partitions]]
name = "default"
default = true
nodes = "ALL"            # accepts every registered node

[partitions.fabric]
isolation = "job"        # every job gets its own FabricDomain via AFM
```

Users who submit without `--partition` get per-job isolation automatically. Users who need a different isolation model submit to a named partition explicitly.

### Config

Each rack has its own AFM controller. The cluster config defines racks separately from partitions — racks express physical fabric topology, partitions express scheduling and isolation policy. The scheduler joins them at dispatch time by mapping each scheduled node to its rack:

```toml
[fabric]
plugin = "amd_afm"    # or "nvidia_imex" or "none"

[[fabric.rack]]
name = "rack1"
nodes = "node[1-8]"
afm_endpoint = "http://afm.rack1.cluster:8080"

[[fabric.rack]]
name = "rack2"
nodes = "node[9-16]"
afm_endpoint = "http://afm.rack2.cluster:8080"
```

Partitions declare their isolation scope independently of the rack config:

```toml
# Default partition — per-job isolation for all unspecified jobs
[[partitions]]
name = "default"
default = true
nodes = "ALL"
[partitions.fabric]
isolation = "job"

# Shared partition — one FabricDomain per partition, all jobs share it
# Spur still creates this via AFM; it is created when the first job
# arrives and destroyed when the partition goes idle
[[partitions]]
name = "gpu-shared"
nodes = "node[1-8]"
[partitions.fabric]
isolation = "partition"

# Reservation partition — FabricDomain created at reservation time
[[partitions]]
name = "gpu-reserved"
nodes = "node[9-16]"
[partitions.fabric]
isolation = "reservation"

# Externally managed — Spur uses the pre-existing vPoD ID as-is, calls no AFM
[[partitions]]
name = "gpu-external"
nodes = "node[17-24]"
[partitions.fabric]
vpod_id = "vpod-precreated-abc"

# No fabric — CPU-only or scale-out-only nodes
[[partitions]]
name = "cpu"
nodes = "cpu[1-32]"
# no [partitions.fabric] section
```

When a job is scheduled, the controller looks up the partition's `isolation` mode, derives the `FabricDomainSpec` from the scheduled nodes and their rack memberships, and calls the appropriate `FabricPlugin` method. The `isolation = "partition"` case creates the domain lazily on first job and reuses it for subsequent jobs in that partition; `isolation = "job"` creates and destroys a domain for every job.

### User-facing visibility

**`sinfo`** — fabric mode is shown as a partition attribute so users know the isolation model before submitting:

```
PARTITION      AVAIL  NODES  FABRIC_ISOLATION  FABRIC_DOMAIN
default        up     32     job               (per-job)
gpu-shared     up     8      partition         fd-p1q2r3
gpu-reserved   up     8      reservation       (per-reservation)
gpu-external   up     8      external          vpod-abc123
cpu            up     32     none              -
```

**`squeue`** — fabric domain is shown per running job so users can verify what isolation they have:

```
JOBID  PARTITION    USER   ST  NODES  FABRIC_DOMAIN   ISOLATION
1234   default      alice  R   4      fd-x7y8z9       job
1235   gpu-shared   bob    R   2      fd-p1q2r3       partition (shared)
1236   gpu-shared   carol  R   4      fd-p1q2r3       partition (shared)
1237   gpu-reserved dave   R   8      fd-r4s5t6       reservation
```

Jobs 1235 and 1236 are explicitly shown sharing the same FabricDomain — no ambiguity. Job 1234 has its own.

### Asserting isolation requirements at submit time

A user who needs per-job isolation can assert it defensively with `--fabric-isolation=required`:

```bash
sbatch --partition=gpu-shared --fabric-isolation=required job.sh
# error: partition gpu-shared provides partition-level fabric isolation.
#        Per-job isolation requires isolation=job or isolation=reservation.
#        Use --partition=default or request a reservation.
```

This prevents the silent failure mode where a user submits to the wrong partition and gets no isolation. The flag causes a hard rejection at submit time with a clear message, rather than the job running unprotected.

### Environment injection

Jobs receive enough information to let RCCL and MPI select the right communication path for each peer:

```
SPUR_FABRIC_DOMAIN_ID=fd-xyz789          # stable domain identifier
SPUR_VPOD_ID=vpod-abc123                 # this node's scale-up vPoD
SPUR_SCALE_UP_PEERS=node1,node2          # nodes sharing this node's vPoD (NVLink/IFoE path)
SPUR_SCALE_OUT_PEERS=node5,node6         # nodes in other racks (AINIC/RoCEv2 path)
```

`SPUR_SCALE_OUT_PEERS` is empty for single-rack jobs. RCCL can use `SPUR_SCALE_UP_PEERS` to identify ranks that share the high-bandwidth scale-up fabric, and `SPUR_SCALE_OUT_PEERS` to identify ranks that require the scale-out path. This provides what AIFM topology injection is intended to provide (currently still under specification), without waiting for that feature to ship.

### Container jobs and AINIC device injection

Spur container jobs run with host networking — no new network namespace is created (`CLONE_NEWNET` is never called). AINIC IPs are pre-configured on the host and are visible inside containers without any additional setup.

However, RDMA operations (used by RoCEv2 over AINICs) require character device files under `/dev/infiniband/` — `uverbs0`, `rdma_cm`, and related nodes. Spur currently injects GPU device nodes (`/dev/kfd`, `/dev/dri/renderD*`, `/dev/nvidia*`) into containers but has no handling for `/dev/infiniband/`. A containerized job that calls `ibv_open_device()` will fail even though the AINIC's network interface is reachable.

`RLIMIT_MEMLOCK` is already raised in both container and non-container paths (for `ibv_reg_mr` compatibility), so that prerequisite is met. The missing piece is device node injection.

For multi-rack containerized jobs, the `ContainerInjectionPlan` needs to include:

```
/dev/infiniband/uverbs0    # per AINIC — needed by libibverbs for RDMA verbs
/dev/infiniband/rdma_cm    # RDMA connection manager — shared across devices
```

These would be added to the injection path in [spur-devices/src/inject.rs](../crates/spur-devices/src/inject.rs) alongside GPU devices, conditioned on the job having a multi-rack `FabricDomain`. Non-container and single-rack jobs are unaffected — host networking and no network namespace means AINICs are already fully accessible.

---

## Priority Order

| Priority | Gap | Why |
|---|---|---|
| 1 | Hard block boundary enforcement | Correctness: without this, NVLink locality is silently violated |
| 2 | Segment abstraction | Utilization: without this, every job wastes a full block or locality breaks |
| 3 | Per-partition topology config | Operability: needed for any mixed cluster with NVL72 and non-NVL72 nodes |
| 4 | IMEX lifecycle hooks | Correctness: without this, cross-node NVLink memory semantics don't work on GB200 |
| 5 | Incomplete block support | Resilience: node failures should not make entire blocks unschedulable |
| 6 | NVLink topology env vars | Usability: NCCL and frameworks need this for optimal collective routing |
