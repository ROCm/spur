# Native-host scheduler perf tests

Optional `@pytest.mark.perf` suite for submit/drain throughput and latency on a
bare-metal Spur cluster. Shell benchmarks live under [`scripts/`](scripts/);
Python orchestration, JSON types, and comparison live in this package.

| Path | Role |
|------|------|
| [`scripts/run_perf.sh`](scripts/run_perf.sh) | One tier on an existing cluster; emits `PERF_METRICS_JSON={...}` |
| [`scripts/run_all.sh`](scripts/run_all.sh) | Multi-tier sweep on one controller (warm state between tiers) |
| [`harness.py`](harness.py) | Deploy cluster, upload `run_perf.sh`, run tiers (cold redeploy between sizes) |
| [`report.py`](report.py) | Suite JSON I/O, markdown summary, PR-vs-nightly comparison |
| [`run_perf_compare.sh`](run_perf_compare.sh) | Runner script: candidate binaries → baseline binaries → compare |
| [`test_scheduler_perf.py`](test_scheduler_perf.py) | Pytest entry point |

## Layout rationale

Perf code is a sibling of `e2e/` under `tests/native_host/`, sharing
[`cluster.py`](../cluster.py) and [`conftest.py`](../conftest.py). JSON
artifacts and `python -m perf.report` replace ad-hoc stdout KEY=VALUE
parsing. `run_all.sh` remains for quick manual multi-tier sweeps on a long-lived
cluster where warm state is acceptable.

## Pytest

Default E2E CI excludes perf: `pytest tests/native_host/e2e/ -m "not perf"`.

```bash
export SPUR_TEST_NODES=10.11.97.24,10.11.97.25,10.11.97.26
export SPUR_TEST_SSH_USER=vm
export SPUR_TEST_SSH_PASSWORD=vm
export SPUR_TEST_BINARIES_DIR=/path/to/release/bin

pytest -c tests/pytest.ini tests/native_host/perf/ -v -m perf -s \
  --perf-json=/tmp/perf-local.json
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SPUR_PERF_TIERS` | `50` | Space- or comma-separated job counts per tier |
| `SPUR_PERF_PARALLEL` | `32` | Parallel submitters (capped per tier) |
| `SPUR_PERF_SLEEP` | `0` | Job sleep seconds passed to `run_perf.sh` |
| `SPUR_PERF_RUN_LABEL` | `local` | Label stored in suite JSON |
| `SPUR_PERF_SCRIPTS_DIR` | `perf/scripts` | Override path to `run_perf.sh` |
| `SPUR_PERF_COMPARE_THRESHOLD_PCT` | `10` | Regression/improvement threshold for compare |

Use `--perf-json=PATH` (not an environment variable) to write suite JSON from pytest.

## Compare two binary trees

```bash
chmod +x tests/native_host/perf/run_perf_compare.sh

tests/native_host/perf/run_perf_compare.sh /tmp/pr-bins /tmp/nightly-bins \
  --candidate-remote-dir /tmp/spur-ci-bin-pr \
  --baseline-remote-dir /tmp/spur-ci-bin-nightly \
  --candidate-label pr-382 \
  --baseline-label nightly \
  --candidate-json /tmp/perf-pr.json \
  --baseline-json /tmp/perf-nightly.json \
  --threshold 10 \
  --fail-on-regression
```

Or compare existing JSON files:

```bash
python -m perf.report /tmp/perf-pr.json /tmp/perf-nightly.json --fail-on-regression
```

## CI

Manual workflow [`.github/workflows/perf.yml`](../../../.github/workflows/perf.yml)
builds PR and nightly binaries, then calls `run_perf_compare.sh`. First merge of
this layout must land before the workflow paths resolve on the default branch.

## Further reading

- Metric definitions: header comment in [`scripts/run_perf.sh`](scripts/run_perf.sh)
- Ingestion benchmark doc: [`docs/developer/ingestion-benchmark.md`](../../../docs/developer/ingestion-benchmark.md)
