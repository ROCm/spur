# Tests

Test suites organized by deployment target. See [docs/developer/building.rst](../docs/developer/building.rst) for setup and usage.

| Path | Description |
|------|-------------|
| `native_host/e2e/` | Deploys Spur on bare-metal nodes via SSH. Includes the WireGuard mesh tests (`test_wg_mesh.py`, `test_wg_k0s.py`) |
| `k8s/e2e/` | Deploys Spur into a Kubernetes cluster via SpurJob CRDs |
