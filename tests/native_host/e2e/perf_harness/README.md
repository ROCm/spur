# perf-harness

Pytest wrapper around [`perf_tests/run_perf.sh`](../../../../perf_tests/run_perf.sh).
Deploys an ephemeral cluster via the E2E `SpurCluster` fixture (controller = node 0),
uploads the shell script, runs tiers over SSH, and parses the stdout metrics block.

**What is measured and how:** [`perf_tests/README.md`](../../../../perf_tests/README.md)
and the [`run_perf.sh`](../../../../perf_tests/run_perf.sh) header comment.

## How the harness runs

1. Upload `run_perf.sh` to the controller node's remote work dir.
2. SSH `exec` on node 0 with `SPUR_CONTROLLER_ADDR` and `SPUR_CLI` set to the
   ephemeral cluster.
3. Parse stdout into `PerfTierResult`; optionally write JSON (`SPUR_PERF_JSON_OUT`).

Entry point: `tests/native_host/e2e/test_scheduler_perf.py` (`@pytest.mark.perf`).
Default E2E CI excludes perf: `pytest …/native_host/e2e/ -m "not perf"`.

## Environment variables (harness only)

| Variable | Default | Description |
|----------|---------|-------------|
| `SPUR_PERF_TIERS` | `50` | Space- or comma-separated job counts |
| `SPUR_PERF_PARALLEL` | `32` | Parallel submitters (capped at tier size) |
| `SPUR_PERF_SLEEP` | `0` | Job sleep seconds (passed to `run_perf.sh`) |
| `SPUR_PERF_SCRIPTS_DIR` | `{repo}/perf_tests` | Directory containing `run_perf.sh` |
| `SPUR_PERF_RUN_LABEL` | `local` | Report label (e.g. `pr-123`, `nightly`) |
| `SPUR_PERF_JSON_OUT` | _(unset)_ | Write suite JSON to this path |
| `SPUR_PERF_COMPARE_THRESHOLD_PCT` | `10` | Regression threshold for `compare.py` |

Shell-script variables (`SPUR_CONTROLLER_ADDR`, `PERF_JOB_NAME`, etc.) are set by
the harness or by `run_perf.sh` defaults; see [`perf_tests/README.md`](../../../../perf_tests/README.md).

Cluster deploy (`SPUR_TEST_NODES`, SSH, binary paths) matches the native-host E2E
suite — see `docs/developer/building.rst`.

## Run locally

```bash
export SPUR_TEST_NODES=10.0.1.10,10.0.1.11,10.0.1.12
export SPUR_TEST_SSH_USER=vm

pytest tests/native_host/e2e/test_scheduler_perf.py -v -m perf -s

SPUR_PERF_TIERS="100 500" SPUR_PERF_PARALLEL=48 \
  pytest tests/native_host/e2e/test_scheduler_perf.py -v -m perf -s
```

Use `pytest -s` to see the markdown summary printed by the test.

## CI: PR vs nightly

[`.github/workflows/perf.yml`](../../../../.github/workflows/perf.yml) (`workflow_dispatch`
only): build PR binaries, download latest `nightly` release, run perf twice
(`SPUR_PERF_RUN_LABEL=pr-<n>` and `nightly`), then `perf_harness.compare` prints
a markdown diff to the workflow log. No artifacts uploaded.

Compare two JSON files manually:

```bash
python -m perf_harness.compare /tmp/candidate.json /tmp/baseline.json
```
