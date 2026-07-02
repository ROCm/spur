# Spur performance tests

Shell benchmarks for scheduling and ingestion on a Spur cluster: submit throughput,
`SubmitJob` RPC handle time, and queue-wait latency after held jobs are released.

**Pipeline, timeline, and metric formulas:** see the header comment in
[`run_perf.sh`](run_perf.sh) (single source of truth).

| File | Purpose |
|------|---------|
| [`run_perf.sh`](run_perf.sh) | One tier; stdout KEY=VALUE metrics block |
| [`run_all.sh`](run_all.sh) | Several tiers; combined summary table on stdout |

## Manual run (existing cluster)

Run on a node where `spur` can reach spurctld (`SPUR_CONTROLLER_ADDR`).

```bash
cd perf_tests
chmod +x run_perf.sh run_all.sh

# One tier: N jobs, sleep seconds, parallel submitters
./run_perf.sh 1000 0 48

# Multi-tier sweep
TIERS="100 500 1000 2000" PAR=48 SLEEP=0 ./run_all.sh
```

Copy to a remote node if needed: `scp run_perf.sh run_all.sh <host>:~/`

## Environment variables (`run_perf.sh` / `run_all.sh`)

| Variable | Default | Description |
|----------|---------|-------------|
| `SPUR_CONTROLLER_ADDR` | `http://localhost:6817` | spurctld gRPC (Raft **leader** in HA) |
| `SPUR_CLI` | `$HOME/spur/bin/spur` | Spur CLI binary |
| `TIERS` | `100 500 1000 2000` | Job counts per tier (`run_all.sh`) |
| `SLEEP` | `0` | Seconds each job sleeps in the script |
| `PAR` | `32` | Parallel submitters |
| `DRAIN_TIMEOUT` | `1200` | Max seconds to wait for queue drain |
| `SAMPLE_MAX` | `100` | Max jobs sampled for latency percentiles |
| `PERF_JOB_NAME` | `spur_perf_<pid>` | Job name for the tier; `scancel -n` cleanup target |

## Pytest and CI

For ephemeral-cluster runs, JSON export, PR-vs-nightly comparison, and `SPUR_PERF_*`
variables, see [`tests/native_host/e2e/perf_harness/README.md`](../tests/native_host/e2e/perf_harness/README.md).
