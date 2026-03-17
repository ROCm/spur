# Spur Implementation Roadmap

AI-native job scheduler. 314 tests passing, end-to-end job dispatch working, WireGuard mesh networking operational.

## Completed

| Phase | Summary |
|-------|---------|
| 1-5 | Core: job submission, scheduling, dispatch, state persistence, CLI |
| 6 | Dependencies, GRES scheduling, hold/release, node health |
| 7 | Job arrays, JWT auth, preemption |
| 8 | sacctmgr, QOS enforcement, TRES, accounting |
| 9 | srun, job steps, consumable TRES, CPU/GPU binding |
| 10 (partial) | WireGuard mesh networking, multi-node dispatch, agent self-reporting |

## Phase 10: Production Hardening

| # | Item | Size | Description |
|---|------|------|-------------|
| 10.1 | HA controller failover | XL | openraft for bare-metal, K8s Lease for cloud |
| 10.2 | Reservations | L | Admin time+node reservations |
| 10.3 | REST API expansion | L | ~30 more endpoints for full slurmrestd compat |
| 10.4 | sdiag/sprio/sshare | S | Scheduler diagnostics, priority breakdown, fair-share tree |
| 10.5 | Container support | L | OCI/Singularity/Enroot integration |
| 10.6 | slurm.conf parser | M | Key=value format for migration from Slurm |
| 10.7 | PMI key-value server | XL | PMI-2 C API for MPI rank wireup over WireGuard mesh |

## Phase 11: Kubernetes Integration

Spur becomes the scheduling brain for GPU workloads across both bare-metal and Kubernetes. The goal is NOT to replace the K8s scheduler for general workloads — it's to provide GPU-aware, topology-aware, gang-scheduling capabilities that K8s lacks natively.

### Architecture

```
                        ┌──────────────────────┐
                        │       spurctld        │
                        │  (scheduling brain)   │
                        └──────┬───────┬────────┘
                               │       │
              ┌────────────────┘       └────────────────┐
              ▼                                         ▼
   ┌──────────────────┐                    ┌────────────────────┐
   │  Bare-metal nodes │                    │  Kubernetes cluster │
   │  spurd agents     │                    │  spur-k8s operator  │
   │  WireGuard mesh   │                    │  CRDs + pod mgmt    │
   └──────────────────┘                    └────────────────────┘
```

### 11.1 SpurJob CRD + Operator (L)

Custom Resource Definition that maps Spur jobs to K8s resources:

```yaml
apiVersion: spur.ai/v1
kind: SpurJob
metadata:
  name: training-run-42
spec:
  nodes: 4
  tasksPerNode: 8
  gres: "gpu:mi300x:8"
  timeLimit: "4:00:00"
  image: "registry.example.com/train:latest"
  script: |
    torchrun --nproc_per_node=8 train.py
```

The operator watches SpurJob CRDs, translates them to Spur job submissions, and creates Pods when spurctld schedules the job. This means K8s users get Spur's scheduling (gang scheduling, GPU topology, fair-share) without learning a new CLI.

### 11.2 Virtual Kubelet Provider (XL)

A virtual-kubelet implementation that registers bare-metal nodes as K8s nodes. GPU workloads scheduled by K8s land on Spur-managed bare-metal via the virtual kubelet, which translates pod specs to `LaunchJobRequest`.

This bridges the gap: K8s clusters can burst to bare-metal GPU nodes managed by Spur, and bare-metal clusters can accept work from K8s.

### 11.3 Node Pool Unification (L)

A single `spur nodes` view shows both bare-metal (spurd agents) and K8s nodes (operator-reported). The scheduler treats them uniformly — same priority, same fair-share, same topology awareness.

Config:
```toml
[kubernetes]
enabled = true
kubeconfig = "/etc/spur/kubeconfig"
namespace = "spur-jobs"
node_selector = { "spur.ai/managed": "true" }
```

### 11.4 GPU Topology Scheduling (L)

Leverage AMD's XGMI/InfinityFabric and NVIDIA's NVLink topology for placement. When a 4-GPU job arrives, prefer 4 GPUs on the same XGMI hive rather than 4 random GPUs across PCIe.

Topology is discovered by spurd at registration (already have `peer_gpus` and `link_type` in the proto). The scheduler needs to use this during assignment.

### 11.5 Gang Scheduling (M)

All-or-nothing scheduling for multi-node jobs. Currently the scheduler assigns nodes independently — if only 3 of 4 required nodes are available, it assigns 3 and the job partially starts. Gang scheduling holds the assignment until all nodes are available.

This is critical for distributed training where partial starts waste resources.

## Phase 12: Training Workloads

First-class support for distributed training jobs — the primary use case for GPU clusters.

### 12.1 Elastic Training (XL)

Jobs that can scale up/down while running. When nodes become available, add them to a running training job. When preempted, shrink gracefully.

Integration points:
- **PyTorch Elastic (torchrun)**: Spur manages the rdzv backend, dynamically updates `--nnodes` range
- **DeepSpeed**: Spur sets `MASTER_ADDR`, `MASTER_PORT`, `WORLD_SIZE`, `RANK` per node
- **JAX**: Spur provides the coordinator address and device mesh info

Environment variables injected by Spur (already partially implemented):
```
SPUR_NUM_NODES=4
SPUR_TASK_OFFSET=0
SPUR_PEER_NODES=10.44.0.2:6818,10.44.0.3:6818,...
MASTER_ADDR=10.44.0.2       # first node in allocation
MASTER_PORT=29500
WORLD_SIZE=32                # num_nodes * tasks_per_node
RANK=0                       # global rank (task_offset based)
LOCAL_RANK=0                 # rank within this node
```

### 12.2 Checkpoint-Aware Scheduling (L)

The scheduler knows about checkpoint intervals. When preempting a training job, it waits for the next checkpoint boundary (or signals the job to checkpoint immediately) rather than killing mid-step.

```bash
#SBATCH --signal=USR1@120    # Send USR1 120s before time limit
#SBATCH --requeue            # Auto-requeue after checkpoint
```

Spur sends the signal, the training framework saves a checkpoint, and the job is requeued with higher priority.

### 12.3 Multi-Job Training Pipelines (M)

Declarative pipelines for common training patterns:

```yaml
# spur-pipeline.yaml
stages:
  - name: preprocess
    nodes: 2
    script: preprocess.sh
  - name: train
    nodes: 8
    gres: "gpu:mi300x:8"
    script: train.sh
    depends_on: preprocess
  - name: eval
    nodes: 1
    gres: "gpu:mi300x:1"
    script: eval.sh
    depends_on: train
```

This is syntactic sugar over job dependencies (already implemented) but provides a better UX for ML workflows.

### 12.4 Experiment Tracking Integration (S)

Inject Spur job metadata into experiment trackers:

```bash
# Automatically set by Spur
SPUR_JOB_ID=42
SPUR_JOB_NAME=gpt-finetune
SPUR_CLUSTER=gpu-cluster
SPUR_ALLOCATED_GPUS=mi300x:0,1,2,3,4,5,6,7

# Frameworks like W&B/MLflow pick these up
WANDB_RUN_ID=spur-42
MLFLOW_RUN_NAME=spur-42-gpt-finetune
```

### 12.5 Dataset/Model Cache Locality (M)

Prefer scheduling jobs on nodes that already have the required dataset or model weights cached in local NVMe. Spurd reports cached datasets as a node feature:

```
FEATURES=mi300x,nvme2tb,cached:llama-70b,cached:openwebtext
```

The scheduler uses `--constraint=cached:llama-70b` to prefer (not require) nodes with warm caches.

## Phase 13: Inference Workloads

Inference has fundamentally different scheduling requirements from training: low latency, autoscaling, request routing, and long-running services rather than batch jobs.

### 13.1 Service Jobs (L)

A new job type that runs indefinitely (or until cancelled), with health checks and auto-restart:

```bash
#SBATCH --job-name=llama-serve
#SBATCH --gres=gpu:mi300x:4
#SBATCH --service                    # New flag: service mode
#SBATCH --health-check=http://localhost:8000/health
#SBATCH --health-interval=30

vllm serve meta-llama/Llama-3.1-70B --tensor-parallel-size 4
```

Service jobs:
- Don't have a time limit (run until cancelled)
- Have health checks (HTTP or TCP)
- Auto-restart on failure (with backoff)
- Get a stable DNS name on the WireGuard mesh: `llama-serve.spur.local`

### 13.2 GPU Fractional Sharing (L)

Multiple inference services on one GPU using MIG (NVIDIA) or compute partitions (AMD):

```bash
#SBATCH --gres=gpu:mi300x:1g.48gb    # 1 MIG slice, 48GB
#SBATCH --gres=gpu:mi300x:0.5        # Half a GPU (time-sliced)
```

The scheduler tracks fractional GPU allocations and packs multiple services per GPU when partitioning is available.

### 13.3 Autoscaling (XL)

Scale inference services based on request load:

```toml
# In job spec or config
[autoscale]
min_replicas = 1
max_replicas = 8
metric = "requests_per_second"
target = 100
scale_up_cooldown = "60s"
scale_down_cooldown = "300s"
```

Spur monitors the metric (scraped from the service's health endpoint or a Prometheus-compatible endpoint), then submits/cancels replica jobs automatically.

On bare-metal: replicas are additional Spur jobs on available nodes.
On K8s: replicas are additional pods via the operator.

### 13.4 Request Routing (L)

A lightweight load balancer that routes inference requests to the right service replicas:

```
Client → spur-router (10.44.0.1:8080)
           ├─→ llama-serve replica 0 (10.44.0.2:8000)
           ├─→ llama-serve replica 1 (10.44.0.3:8000)
           └─→ llama-serve replica 2 (10.44.0.4:8000)
```

The router is a small binary (`spur-router`) that reads the service registry from spurctld and does weighted round-robin. Model-specific routing (e.g., different models on different replicas) is supported via path-based routing.

### 13.5 Model Serving Templates (S)

Pre-built job templates for common serving frameworks:

```bash
# One-liner to serve a model
spur serve vllm meta-llama/Llama-3.1-70B --gpus 4 --replicas 2
spur serve tgi mistralai/Mixtral-8x7B --gpus 8
spur serve triton models/ --gpus 1
```

These generate the appropriate `#SBATCH` directives and launch commands for vLLM, TGI, Triton, etc.

## Phase 14: Observability + Multi-Cluster

### 14.1 Prometheus Metrics (M)

Export scheduler, node, and job metrics in Prometheus format:

```
spur_jobs_total{state="running",partition="gpu"} 42
spur_node_gpu_utilization{node="gpu-001",gpu="0"} 0.87
spur_scheduler_cycle_duration_seconds 0.003
spur_queue_wait_seconds{partition="gpu",quantile="0.99"} 120
```

### 14.2 Multi-Cluster Federation (XL)

Multiple Spur clusters with job forwarding:

```toml
[[federation.clusters]]
name = "us-east"
controller = "10.44.0.1:6817"

[[federation.clusters]]
name = "eu-west"
controller = "10.45.0.1:6817"
```

Submit to any cluster, jobs route to the one with available GPUs. Cross-cluster WireGuard mesh for data plane.

### 14.3 Cost Tracking (M)

Per-job GPU-hour costs for chargeback:

```bash
spur history --format=cost
JOBID  USER   GPUS     HOURS   COST
42     alice  mi300x:8  4.2    $33.60
43     bob    mi300x:4  12.0   $48.00
```

Configurable per-GPU-type rates. Feeds into accounting for budget enforcement.

## Priority Order

**Now → 3 months:**
- Phase 10 (production hardening) — HA, reservations, PMI
- Phase 11.1-11.3 (K8s basics) — CRD, operator, node unification
- Phase 12.1-12.2 (training) — elastic training, checkpoint-aware scheduling

**3-6 months:**
- Phase 11.4-11.5 (K8s advanced) — topology, gang scheduling
- Phase 12.3-12.5 (training UX) — pipelines, experiment tracking, cache locality
- Phase 13.1-13.2 (inference basics) — service jobs, GPU sharing

**6-12 months:**
- Phase 13.3-13.5 (inference scaling) — autoscaling, routing, templates
- Phase 14 (observability + multi-cluster)

## Alternatives Considered

**Replace K8s scheduler entirely:** Too ambitious, breaks existing K8s workloads. Better to coexist — Spur handles GPU scheduling, K8s handles everything else.

**Inference as a separate system (like KServe/Seldon):** Fragmenting GPU management across systems leads to utilization problems. A single scheduler that understands both training and inference can pack the cluster better (shift GPUs from idle inference to training overnight, etc).

**Build on Ray/Dask instead of bare-metal agents:** These are good for distributed compute but don't provide the low-level node management (cgroups, WireGuard, GPU isolation) needed for multi-tenant clusters. Spur is lower in the stack.
