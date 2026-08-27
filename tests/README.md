# Tests

Test suites organized by deployment target. See [docs/developer/building.rst](../docs/developer/building.rst) for setup and usage.

| Path | Description |
|------|-------------|
| `native_host/e2e/` | Deploys Spur on bare-metal nodes via SSH. Includes the WireGuard mesh tests (`test_wg_mesh.py`, `test_wg_k0s.py`, marked `wireguard`), which auto-run where the nodes allow — the fixtures install WireGuard if missing and skip only where a data plane can't be provided |
| `k8s/e2e/` | Deploys Spur into a Kubernetes cluster via SpurJob CRDs |
