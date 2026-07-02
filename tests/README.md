# Tests

Test suites organized by deployment target. See [docs/developer/building.rst](../docs/developer/building.rst) for setup and usage.

| Path | Description |
|------|-------------|
| `native_host/e2e/` | Deploys Spur on bare-metal nodes via SSH |
| `native_host/e2e/perf_harness/` | Helpers for optional `@pytest.mark.perf` scheduler perf tests |
| `k8s/e2e/` | Deploys Spur into a Kubernetes cluster via SpurJob CRDs |
