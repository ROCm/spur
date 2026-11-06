# 30-arc — install order (ARC controller → GitHub App secret → runner scale sets)

Pin every chart version in `../versions.lock.md`. Uninstall in REVERSE order (scale sets before
controller) or finalizers hang — see `../teardown.sh`.

## 1. Controller (once per cluster)

```bash
helm install arc -n arc-systems --create-namespace \
  oci://ghcr.io/actions/actions-runner-controller-charts/gha-runner-scale-set-controller   # --version PIN
kubectl -n arc-systems rollout status deploy/arc-gha-rs-controller
```

## 2. GitHub App secret + image pull secret (namespace arc-runners)

```bash
kubectl create ns arc-runners
# GitHub App creds (see github-app-secret.example.yaml; prefer sealed-secrets/SOPS over kubectl create):
kubectl -n arc-runners create secret generic sglang-arc-app \
  --from-literal=github_app_id=<APP_ID> \
  --from-literal=github_app_installation_id=<INSTALL_ID> \
  --from-file=github_app_private_key=./sglang-app.private-key.pem
# GHCR pull secret for the runner image:
kubectl -n arc-runners create secret docker-registry ghcr-pull \
  --docker-server=ghcr.io --docker-username=<gh-user> --docker-password=<gh-PAT-with-read:packages>
```

## 3. GPU runner scale set (the sglang CI runners)

```bash
helm install linux-mi35x-gpu-1 -n arc-runners -f runner-scale-set-values.yaml \
  oci://ghcr.io/actions/actions-runner-controller-charts/gha-runner-scale-set              # --version PIN
```

## 4. CPU deploy scale set (runs `kubectl apply`, no GPU)

```bash
helm install linux-cpu-deploy -n arc-runners -f cpu-deploy-values.yaml \
  oci://ghcr.io/actions/actions-runner-controller-charts/gha-runner-scale-set              # --version PIN
```

## Verify

```bash
kubectl -n arc-systems get pods                 # controller Running
kubectl -n arc-runners get pods                 # AutoscalingListener(s) Running
# In the fork: Settings → Actions → Runner scale sets shows linux-mi35x-gpu-1 and linux-cpu-deploy.
```

## GitHub App permissions (validate against current ARC docs at install time)

Repository permissions: **Actions: Read**, **Administration: Read & write**, **Metadata: Read**.
Install the App on `powderluv/sglang` only. Record App ID + Installation ID.
