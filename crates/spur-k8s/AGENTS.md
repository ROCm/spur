# AGENTS.md -- spur-k8s

Kubernetes integration crate for Spur. Bridges an existing K8s cluster and the
Spur scheduling controller (`spurctld`) so GPU workloads submitted as SpurJob
CRDs are scheduled by Spur and executed as K8s Pods.

Binary: `spur-k8s-operator`. Library: `spur_k8s`.

## Architecture

The operator runs five concurrent tasks (all under `run_with_retry` with
exponential backoff 1s..60s):

1. **Health server** (axum, `:8080`) -- `/healthz` (liveness), `/readyz`
   (K8s API + spurctld reachability), `/metrics` (Prometheus).
2. **Heartbeat sender** -- 30s interval `Heartbeat` RPCs to spurctld for every
   tracked node. Re-registers a node on `NOT_FOUND` (controller state loss).
3. **Node watcher** -- watches K8s nodes matching `--node-selector`
   (`spur.amd.com/managed=true`). Registers them with spurctld via
   `RegisterAgent`, tracks/untracks in the heartbeat registry. Syncs
   NotReady taint to spurctld `NodeDown`/`NodeIdle`.
4. **Job controller** -- kube-rs `Controller` reconciling SpurJob CRDs.
   Submits to spurctld, polls state, patches CRD status. Owns a Pod watcher
   that reports terminal Pod states back to spurctld.
5. **Quota reconciler** (opt-in, `--enable-quota`) -- projects SPUR accounts
   into per-account Namespace + ResourceQuota + LimitRange + RBAC via
   server-side apply with drift correction.

A **virtual agent** gRPC server (`:6818`) implements `SlurmAgent` and receives
`LaunchJob` / `CancelJob` from spurctld, translating them into Pod
create/delete operations.

## CRD: SpurJob

```
apiVersion: spur.amd.com/v1alpha1
kind: SpurJob
```

Defined in `crd.rs`. Key spec fields: `name`, `image`, `gpus` (count + type),
`numNodes`, `tasksPerNode`, `cpusPerTask`, `memoryPerNode`, `timeLimit`,
`command`, `args`, `env`, `secretEnv`, `volumes`, `hostNetwork`, `privileged`,
`hostIpc`, `shmSize`, `extraResources`, `tolerations`, `nodeSelector`,
`arraySpec`, `dependencies`.

Status subresource: `state`, `spurJobId`, `assignedNodes`, `pods`, `message`.

Generate the CRD YAML: `spur-k8s-operator generate-crd`.

## Job controller (`job_controller.rs`)

Reconcile loop (kube-rs finalizer pattern):

- **Submit phase**: no `spur_job_id` yet -- fresh-reads from API server to
  guard against stale informer cache, submits to spurctld via `SubmitJob`,
  labels the SpurJob with `spur.amd.com/job-id`, patches status.
- **Poll phase**: has `spur_job_id` -- polls `GetJob`, patches CRD status on
  state change.
- **Deletion**: `spur.amd.com/cleanup` finalizer. Cancels the Spur job (retries
  on transport errors to avoid orphans), deletes owned Pods and headless
  Service.

Terminal states: `Completed`, `Failed`, `Cancelled`, `Timeout`, `NodeFail`.

Per-object exponential backoff (1s..60s) keyed by namespace/name/UID.

Pod watcher (label `spur.amd.com/managed-by=spur-k8s-operator`): detects
Succeeded/Failed/OOMKilled/ImagePullBackOff, reports to spurctld via
`ReportJobStatus`. OOM encoded via signal sentinel. Orphan Pod cleanup on
startup.

## Virtual agent (`agent.rs`)

Implements `SlurmAgent` gRPC trait. Key RPCs:

- `LaunchJob` -- resolves SpurJob namespace via label lookup, validates
  `target_node` against the recorded allocation, creates a K8s Pod with
  resource requests (CPU/memory/GPU), env vars (SPUR_* + SLURM_* twins),
  volume mounts (hostPath and PVC), shared memory, security context. For
  multi-node jobs: creates a headless Service for peer DNS discovery
  (`<sanitized-node>.spur-job-<id>.<ns>.svc.cluster.local`).
- `CancelJob` -- deletes all Pods by label + headless Service.
- `ExecInJob` -- `kubectl exec` equivalent via kube-rs attach.
- `StreamJobOutput` -- Pod log streaming.
- Execution credential verification (Ed25519 JWKS) when native auth is
  configured.

GPU vendor detection: AMD GPUs (`mi300x`, `gfx*`, etc.) use `amd.com/gpu`;
NVIDIA GPUs (`h100`, `a100`, etc.) use `nvidia.com/gpu`. Default is AMD.

## Node watcher (`node_watcher.rs`)

Watches nodes matching the label selector. Extracts resources from
`status.allocatable`: CPU (whole cores, millicore-aware), memory, GPU count
(`amd.com/gpu` preferred over `nvidia.com/gpu`). GPU metadata from labels:
`spur.amd.com/gpu-type`, `spur.amd.com/gpu-memory-mb`, `spur.amd.com/gpu-link`
(xgmi/nvlink/pcie).

Fingerprint-based re-registration: only re-registers when resources change.
Taint tracking: `node.kubernetes.io/not-ready` NoSchedule maps to spurctld
`NodeDown`; removal maps to `NodeIdle`. Unsynced taint states retry on next
event.

## Controller client (`controller.rs`)

Single long-lived gRPC channel to spurctld with:
- 10s connect timeout, 30s request timeout
- HTTP/2 keepalive (10s interval, 5s timeout)
- Auto-reconnect on transport errors (retryable status drops the channel)
- `probe()` for readiness checks

## Quota management (`quota.rs`, `quota_controller.rs`)

Opt-in (`--enable-quota`). Projects SPUR accounts into K8s tenancy objects:

- **Namespace**: `spur-acct-<account>`, PodSecurity Admission `baseline`
- **ResourceQuota**: `spur-account-quota`, maps `grp_tres` (cpu/memory/gpu)
  to `requests.*` and `limits.*` hard caps. Zero allocation = `pods: 0`.
- **LimitRange**: `spur-account-defaults`, default container
  requests+limits (100m CPU, 128M memory) for Guaranteed QoS.
- **Role**: `spur-account-editor`, workload management verbs on core/batch/apps
  resources.
- **RoleBinding**: `spur-account-members`, binds per-user ServiceAccounts.

All objects labeled `app.kubernetes.io/managed-by: spur-quota`. Server-side
apply with `force` reverts admin hand-edits. 30s reconcile interval.
`ListAccounts` + `ListUsers` from spurctld accounting gRPC.

## Auth middleware (`auth_middleware.rs`)

Tower layer on the virtual agent gRPC server. Three modes (`--auth-mode`):
- `disabled` -- no verification.
- `permissive` (default) -- verifies presented credentials, allows anonymous.
- `required` -- rejects uncredentialed calls.

Uses `spur_core::auth::BearerAuth`. JWT key from `--jwt-key` / `SPUR_JWT_KEY`
or native JWKS from spur.conf. Forged credentials are logged with peer address.

## Key labels

| Label | Purpose |
|---|---|
| `spur.amd.com/managed=true` | Node selector for the watcher |
| `spur.amd.com/job-id` | Links Pods/Services to a SpurJob |
| `spur.amd.com/managed-by=spur-k8s-operator` | Owned Pod identification |
| `spur.amd.com/target-node` | Pod placement tracking |
| `spur.amd.com/gpu-type` | GPU type for resource inventory |
| `spur.amd.com/gpu-memory-mb` | GPU memory for resource inventory |
| `spur.amd.com/gpu-link` | GPU interconnect (xgmi/nvlink) |

## CLI flags

| Flag | Default | Purpose |
|---|---|---|
| `--controller-addr` | `localhost:6817` | spurctld gRPC address |
| `--listen` | `[::]:6818` | Virtual agent listen address |
| `--address` | POD_IP / hostname | Advertised address to spurctld |
| `--node-selector` | `spur.amd.com/managed=true` | K8s node label filter |
| `--enable-quota` | false | Enable quota policy reconciler |
| `--health-addr` | `[::]:8080` | Health/metrics server |
| `--auth-mode` | `permissive` | Agent auth strictness |
| `--jwt-key` | (none) | Cluster JWT signing key |
| `--config` | `/etc/spur/spur.conf` | Spur config file |

## Testing

`test_support.rs` provides `FakeApiServer` -- a `tower::Service` that returns
canned responses and records requests, yielding a `kube::Client` without a
real cluster. Used by `resolve_job_tests` in `agent.rs`.

Unit tests cover: backoff arithmetic, terminal state detection, CRD
serialization, resource extraction, GPU vendor detection, name sanitization,
mount parsing, taint detection, fingerprinting, auth middleware decisions,
heartbeat registry, quota projection.

## Dependencies

Core: `kube`, `k8s-openapi`, `tonic`, `axum`, `tokio`, `clap`.
Internal: `spur-core` (job types, auth, config), `spur-proto` (gRPC stubs).
