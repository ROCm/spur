# Spur Helm Chart

Spur — an AI-native, Slurm-compatible job scheduler — packaged as a Helm chart.

## TL;DR

```bash
helm install spur ./deploy/helm/spur \
  --namespace spur --create-namespace \
  --set image.repository=ghcr.io/rocm/spur \
  --set image.tag=0.1.0
```

## What gets deployed

| Component | Kind | Default replicas | Purpose |
|---|---|---|---|
| `spurctld` | StatefulSet (headless) | 3 | Controller + Raft consensus + scheduler |
| `spurd` | DaemonSet | per node | Node agent on every compute node |
| `spurrestd` | Deployment | 2 | Slurm-compatible REST API |
| `spurdbd` | Deployment | 1 | Accounting (PostgreSQL) |
| `spur-k8s-operator` | Deployment | 1 | Reconciles `SpurJob` CRs into pods |
| `SpurJob` CRD | CRD (`crds/`) | — | Kubernetes-native job submission |

All components share the same image (multi-call binary). Pick what you need with `<component>.enabled`.

## Requirements

- Kubernetes ≥ 1.25
- Helm ≥ 3.8
- Default StorageClass available (for the controller PVC) — or set `controller.persistence.storageClass`
- For GPU scheduling: nodes labeled `spur.amd.com/compute=true` with ROCm devices (`/dev/kfd`, `/dev/dri`)

## Common overrides

**Single-node dev cluster** — no Raft HA, no persistence, no GPU:

```yaml
controller:
  replicaCount: 1
  persistence:
    enabled: false
  pdb:
    enabled: false
agent:
  nodeSelector: {}      # deploy on every node
  gpu:
    rocm: false
```

**Production with external Postgres**:

```yaml
accounting:
  externalDatabase:
    existingSecret: spur-db-creds   # secret with key "url"
  embeddedPostgres:
    enabled: false
```

**Disable the operator** (you only want the scheduler + CLI, not K8s-native jobs):

```yaml
operator:
  enabled: false
crds:
  install: false
```

## Upgrade & rollback

```bash
helm upgrade spur ./deploy/helm/spur -n spur -f my-values.yaml
helm rollback spur -n spur
```

Config changes roll the controller automatically via `checksum/config` annotation.

## CRDs

The `SpurJob` CRD ships under `crds/`. Helm installs it on first `helm install` but never upgrades or deletes it — that's by design (deleting a CRD wipes every CR). To upgrade the CRD:

```bash
kubectl apply -f deploy/helm/spur/crds/spurjob-crd.yaml
```

If you manage CRDs externally (e.g. ArgoCD pre-sync hook), set `crds.install=false` and ship `crds/spurjob-crd.yaml` yourself.

## Uninstall

```bash
helm uninstall spur -n spur
kubectl delete crd spurjobs.spur.amd.com   # only if you want to drop CRs too
```

## Values reference

See `values.yaml` — every key is documented inline.
