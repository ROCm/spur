# Spur Architecture Overview

**Document Version:** 1.0  
**Last Updated:** June 2026

## What is Spur?

Spur is a modern job scheduler built from the ground up for AI/ML workloads across both bare-metal GPU clusters and Kubernetes. Written in Rust, it solves problems that traditional HPC schedulers like Slurm were never designed for: inference serving, elastic training, and unified scheduling across deployment modes.

**Why Move to Spur?**

Traditional HPC schedulers (Slurm, PBS, LSF) were built for batch scientific computing in the 2000s. They handle training jobs well, but AI/ML infrastructure in 2026 needs more:

1. **Unified Bare-Metal + Kubernetes Scheduling**  
   Run one scheduler for both native-host GPU clusters and Kubernetes. Submit jobs as `sbatch` scripts or K8s CRDs — same queue, same fair-share, same topology awareness. No more "Slurm for training, K8s for inference" split that fragments your GPU utilization.

2. **Native Inference Serving**  
   Long-running service jobs with health checks, autoscaling (scale replicas based on request load), fractional GPU sharing (MIG slices, time-slicing), and request routing. Slurm requires Kubernetes for this; Spur does it natively on bare-metal too.

3. **Elastic Training**  
   Scale running jobs up/down as nodes become available. Add 4 more nodes to a running 8-node training job without restarting. Slurm can burst cluster capacity but can't resize running jobs — elastic training requires research extensions like Invasive MPI.

4. **Zero External Dependencies**  
   Raft-based state replication means no MySQL/PostgreSQL database to maintain. WireGuard mesh networking means no separate VPN infrastructure. Single static Rust binary per component. Slurm needs a database, manual networking setup, and a complex build process.

5. **Modern Codebase**  
   Rust (not C from 2003), async gRPC APIs (not RPC from the '90s), structured logging, Prometheus metrics. Fast builds (30s), safe concurrency, easy to extend.

**Slurm Compatibility is the Migration Path, Not the Reason to Switch**

Your existing `sbatch` scripts, `squeue` muscle memory, REST API clients, and C FFI integrations work unchanged. This removes migration friction — but the reason to move is the features Slurm can't provide: unified bare-metal/K8s scheduling, native inference serving, and elastic training as first-class primitives.

---

## Deployment Modes

### Native-Host (Bare-Metal) Deployment

Traditional HPC-style deployment where Spur runs directly on physical or virtual machines.

**Architecture:**
```
┌─────────────────────────────┐
│   spurctld (controller)     │  Port 6817 (gRPC)
│   - Job scheduling          │  Port 6821 (Raft)
│   - Raft consensus          │
│   - State in /var/spool     │
└──────────┬──────────────────┘
           │ WireGuard mesh (10.44.0.0/16)
      ┌────┴─────┬──────────┐
      ▼          ▼          ▼
┌──────────┐ ┌──────────┐ ┌──────────┐
│  spurd   │ │  spurd   │ │  spurd   │  Port 6818 (gRPC)
│ GPU node │ │ GPU node │ │ GPU node │
└──────────┘ └──────────┘ └──────────┘
```

**Components:**
- **spurctld**: Controller daemon (scheduling brain)
- **spurd**: Node agent (executes jobs, reports resources)
- **spurdbd**: Accounting daemon (optional, PostgreSQL backend)
- **spurrestd**: REST API daemon (optional, Slurm-compatible API)

**Networking:**
WireGuard mesh provides encrypted overlay networking. Agents auto-discover their mesh IP and self-report to the controller. No external VPN setup required.

**Setup:**
```bash
# Controller
spur net init --cidr 10.44.0.0/16
spurctld -f /etc/spur/spur.conf

# Workers
spur net join --endpoint <controller-ip>:51820 --server-key <pubkey>
spurd --controller http://10.44.0.1:6817
```

---

### Kubernetes Deployment

Spur runs inside a Kubernetes cluster with the controller as a StatefulSet and an operator managing job lifecycle.

**Architecture:**
```
┌────────────────────────────────────────┐
│         Kubernetes Cluster              │
│                                         │
│  ┌──────────────────────────────────┐  │
│  │  spurctld StatefulSet (3 pods)   │  │
│  │  - Raft via K8s DNS              │  │
│  │  - PVC for state                 │  │
│  └──────────────────────────────────┘  │
│                                         │
│  ┌──────────────────────────────────┐  │
│  │  spur-k8s-operator               │  │
│  │  - Watches SpurJob CRDs          │  │
│  │  - VirtualAgent (creates pods)   │  │
│  └──────────────────────────────────┘  │
│                                         │
│  ┌──────────────────────────────────┐  │
│  │  Pods (actual job workloads)     │  │
│  └──────────────────────────────────┘  │
└────────────────────────────────────────┘
```

**Key Concept: Virtual Agent**

Instead of running `spurd` on each node, the K8s operator contains a **VirtualAgent** — a component that implements the same `SlurmAgent` gRPC interface but creates Kubernetes Pods instead of forking processes.

From `spurctld`'s perspective, it's talking to a normal agent. The VirtualAgent translates job launch requests into Pod creation via the K8s API.

**Job Submission Flow:**
```
User                spur-k8s-operator      spurctld         VirtualAgent      K8s API
 │                         │                   │                │              │
 │ kubectl apply SpurJob   │                   │                │              │
 ├────────────────────────>│                   │                │              │
 │                         │ SubmitJobRequest  │                │              │
 │                         ├──────────────────>│                │              │
 │                         │                   │ (schedules)    │              │
 │                         │                   │                │              │
 │                         │                   │ LaunchJobReq   │              │
 │                         │                   ├───────────────>│              │
 │                         │                   │                │ Create Pod   │
 │                         │                   │                ├─────────────>│
 │                         │                   │                │              │
 │                         │ Poll status       │                │              │
 │                         │<──────────────────┤                │              │
 │ Update CRD status       │                   │                │              │
 │<────────────────────────┤                   │                │              │
```

**SpurJob CRD Example:**
```yaml
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: training-run
spec:
  script: |
    #!/bin/bash
    #SBATCH --job-name=train
    #SBATCH -N 2
    #SBATCH --gres=gpu:8
    torchrun --nnodes=2 train.py
```

The operator converts this to a Spur job submission, then creates Pods when `spurctld` schedules it.

---

## How Jobs Execute

### Native-Host Job Execution

When `spurctld` schedules a job and dispatches `LaunchJobRequest` to `spurd`, here's what happens:

**1. Process Creation**
```bash
# Job submitted: spur submit train.sh
# On the node, spurd creates:
unshare --pid --mount --fork /bin/bash .spur_ns_42.sh
  └─ /bin/bash /tmp/spur-42-script.sh
      └─ python train.py  # Your actual workload
```

**2. Isolation Layers**

| Layer | Technology | Purpose |
|-------|------------|---------|
| **Namespace** | PID + mount (`unshare(2)`) | Process isolation (optional, requires root) |
| **Cgroup** | `/sys/fs/cgroup/spur/job_42/` | CPU/memory limits, resource tracking |
| **User** | `setuid(alice)` + `initgroups()` | Runs as submitting user, not root |
| **GPU** | `ROCR_VISIBLE_DEVICES=0,1` | Device visibility control |

**3. Environment Variables Injected**
```bash
SPUR_JOB_ID=42
SPUR_NUM_NODES=2
SPUR_TASK_OFFSET=0                          # This node's task offset
SPUR_PEER_NODES=10.44.0.2:6818,10.44.0.3:6818
SPUR_CPUS_ON_NODE=128
ROCR_VISIBLE_DEVICES=0,1,2,3                # AMD GPUs
# or CUDA_VISIBLE_DEVICES=0,1,2,3           # NVIDIA GPUs
MASTER_ADDR=10.44.0.2                       # For PyTorch DDP
MASTER_PORT=29500
WORLD_SIZE=16                               # total tasks
RANK=0                                      # global rank
```

**4. What It Looks Like on the Node**
```bash
# Process tree
ps aux | grep spur
alice  12345  unshare --pid --mount --fork /bin/bash .spur_ns_42.sh
alice  12346   └─ /bin/bash /tmp/spur-42-script.sh
alice  12347       └─ torchrun --nnodes=2 train.py

# Cgroup limits
cat /sys/fs/cgroup/spur/job_42/memory.max
137438953472  # 128GB limit

# Output files
ls /tmp/spur-42.*
/tmp/spur-42.out  /tmp/spur-42.err
```

**Key Point:** A Spur job is **a regular Linux process** with namespace isolation and cgroup limits — not a full container (unless you explicitly request one via `container_image` in the job spec).

---

### Kubernetes Job Execution

In K8s mode, jobs become Pods:

**1. Pod Creation**
When `VirtualAgent.launch_job()` is called, it creates a Pod spec:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: spur-job-42-gpu-node-1
  labels:
    spur.ai/job-id: "42"
spec:
  nodeName: gpu-node-1           # Pin to scheduled node
  restartPolicy: Never
  containers:
  - name: job
    image: busybox:latest        # or user-specified image
    command: ["sh", "-c", "torchrun --nnodes=2 train.py"]
    resources:
      limits:
        amd.com/gpu: "8"
        cpu: "128"
        memory: "512Gi"
    env:
    - name: SPUR_JOB_ID
      value: "42"
    - name: SPUR_PEER_NODES
      value: "10.0.0.2:6818,10.0.0.3:6818"
    - name: MASTER_ADDR
      value: "spur-job-42.default.svc.cluster.local"
```

**2. Scheduling**
- `spurctld` already picked the node (`gpu-node-1`)
- Pod has `nodeName` set, forcing K8s to place it there
- K8s scheduler validates resource availability (GPUs, memory)
- kubelet launches the container

**3. Multi-Node Jobs**
For jobs with `-N 2`, the VirtualAgent:
- Creates a **headless Service** for DNS-based discovery
- Creates **one Pod per node** with unique hostnames
- Sets `MASTER_ADDR` to the Service DNS name
- Each Pod gets its own `RANK` based on `task_offset`

**No spurd daemon runs in K8s mode.** The VirtualAgent + kubelet handle execution.

---

## Key Differences: Native-Host vs Kubernetes

| Aspect | Native-Host | Kubernetes |
|--------|-------------|------------|
| **Job Execution** | `spurd` forks process with `unshare` | VirtualAgent creates Pod via K8s API |
| **Agent** | `spurd` daemon on each node | VirtualAgent in operator (no per-node daemon) |
| **Networking** | WireGuard mesh (self-managed) | K8s networking + optional mesh |
| **State Storage** | Local disk (`/var/spool/spur`) | PersistentVolumeClaim |
| **Job Submission** | CLI (`spur submit job.sh`) | SpurJob CRD (`kubectl apply`) |
| **Isolation** | Namespaces + cgroups | Full container (K8s) |
| **Performance** | Maximum (direct hardware) | Good (minimal container overhead) |
| **Complexity** | Low (2 binaries) | Medium (manifests, CRDs, operator) |

---

## Implementation Status

### ✅ Production-Ready (Native-Host)
- Multi-node job scheduling and dispatch
- WireGuard mesh networking with auto-discovery
- GPU-first scheduling (AMD MI300X, NVIDIA)
- Raft-based HA controller (3-5 nodes)
- Job arrays, dependencies, preemption, QoS
- Slurm CLI compatibility (`sbatch`, `squeue`, `scancel`, etc.)
- Accounting (spurdbd + PostgreSQL)
- Prometheus metrics export
- C FFI (`libspur_compat.so`)
- SPANK plugin support

### ✅ Production-Ready (Kubernetes)
- SpurJob CRD and operator
- VirtualAgent (Pod creation instead of process exec)
- Node watcher (K8s nodes → Spur nodes)
- StatefulSet HA controller
- Job status sync to CRD
- Multi-node jobs with headless Services

### ⏳ Roadmap

**Now → 3 Months (High Priority):**

*Production Hardening (Phase 10)*
- **HA Controller Failover** — openraft for native-host deployments, K8s Lease for cloud. Full failover with Raft log replay and snapshot restore
- **Reservations** — Admin time+node reservations for scheduled maintenance windows
- **REST API Expansion** — ~30 more endpoints for full slurmrestd compatibility
- **sdiag/sprio/sshare** — Scheduler diagnostics, priority breakdown, fair-share tree visualization
- **Container Support** — OCI/Singularity/Enroot integration for containerized jobs
- **slurm.conf Parser** — Key=value format for easier migration from Slurm
- **PMI Key-Value Server** — PMI-2 C API for MPI rank wireup over WireGuard mesh

*K8s Integration Basics (Phase 11.1-11.3)*
- **SpurJob CRD + Operator** — Custom resource for K8s-native job submission, automatic Pod creation when spurctld schedules
- **Node Pool Unification** — Unified view of native-host (spurd) and K8s nodes in `spur nodes`
- **Virtual Agent** — Operator component that implements SlurmAgent interface by creating Pods instead of forking processes

*Training Workloads (Phase 12.1-12.2)*
- **Elastic Training** — Scale jobs up/down while running. Integration with PyTorch Elastic (torchrun), DeepSpeed, and JAX
- **Checkpoint-Aware Scheduling** — Wait for checkpoint boundaries before preemption, auto-requeue with higher priority after checkpoint

**3-6 Months:**
- **GPU Topology Scheduling** — Prefer GPUs on same XGMI/NVLink fabric
- **Gang Scheduling** — All-or-nothing multi-node allocation
- **Training UX** — Pipelines, experiment tracking, dataset cache locality
- **Service Jobs** — Long-running inference with health checks
- **Fractional GPUs** — MIG slices, time-slicing for inference

**6-12 Months:**
- **Virtual Kubelet** — Register native-host nodes as K8s nodes (burst K8s → bare-metal)
- **Autoscaling** — Scale inference replicas based on load
- **Multi-Cluster Federation** — Job forwarding across clusters

---

## Common Questions

**Q: Does Spur replace the Kubernetes scheduler?**  
No. In K8s mode, Spur schedules *which jobs run on which nodes*, then creates Pods with `nodeName` already set. The K8s scheduler just validates placement. Spur coexists with K8s for GPU workloads while K8s handles everything else.

**Q: Can I mix native-host and K8s nodes in one cluster?**  
Not yet. The roadmap includes a **Virtual Kubelet** provider (Phase 11.2) that will register native-host nodes as K8s nodes, enabling unified scheduling across both.

**Q: How does multi-node job communication work?**  
- **Native-host**: WireGuard mesh. All nodes have IPs like `10.44.0.X` and talk directly over the encrypted mesh.
- **K8s**: Headless Service creates DNS names (`spur-job-42-node-0.spur-job-42.ns.svc.cluster.local`). Pods communicate via K8s networking.

**Q: What happens if spurd crashes mid-job?**  
The job process continues running (it's a regular Linux process). When `spurd` restarts, it rediscovers running jobs via cgroup tracking and resumes monitoring. Job state in `spurctld` is Raft-replicated, so no data loss.

**Q: Is Spur only for AI/ML workloads?**  
No. It's a general-purpose job scheduler (Slurm-compatible), but it's *optimized* for GPU workloads with features like GPU topology awareness, distributed training env vars, and inference serving (roadmap).

---

## Resources

- **Repository**: [github.com/ROCm/spur](https://github.com/ROCm/spur)
- **Documentation**: `docs/` directory (quickstart, deployment, building)
- **Roadmap**: `plans/implementation-roadmap.md`
- **License**: Apache 2.0

For deployment instructions, see:
- Native-host: `docs/deployment/native-host.rst`
- Kubernetes: `docs/deployment/kubernetes.rst`
