# Tests

Test suites organized by deployment target. See [docs/developer/building.rst](../docs/developer/building.rst) for setup and usage.

| Path | Description |
|------|-------------|
| `native_host/e2e/` | Deploys Spur on bare-metal nodes via SSH |
| `native_host/wg_e2e/` | Stands up a real WireGuard mesh over SSH nodes; verifies mesh bring-up, k0s-over-mesh, and cross-node pod/service datapath over the tunnel (the 3-controller HA scenario is scaffolded but currently skipped). Opt-in via `SPUR_TEST_WG=1` |
| `k8s/e2e/` | Deploys Spur into a Kubernetes cluster via SpurJob CRDs |
