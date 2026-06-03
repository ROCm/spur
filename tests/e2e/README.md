# E2E Tests

End-to-end test suites for Spur, organized by deployment target:

| Directory | Target | Description |
|-----------|--------|-------------|
| `native_host/` | Native-Host / SSH | Deploys spurctld + spurd on remote nodes via SSH |
| `k8s/` | Kubernetes | Deploys controller + operator into a K8s cluster, submits SpurJob CRDs |

For setup instructions, environment variables, and how to run the tests, see the [Building guide](../../docs/developer/building.rst).

```bash
pip install -r tests/e2e/requirements.txt

# Native-host (requires SPUR_TEST_NODES, SPUR_TEST_SSH_USER, etc.)
pytest tests/e2e/native_host/ -v

# Kubernetes (requires KUBECONFIG, SPUR_CI_IMAGE, etc.)
pytest tests/e2e/k8s/ -v
```
