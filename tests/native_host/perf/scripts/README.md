# Spur performance scripts

Shell benchmarks for scheduling and ingestion on a Spur cluster: submit throughput,
`SubmitJob` RPC handle time, and queue-wait latency after held jobs are released.

**Pipeline, timeline, and metric formulas:** see the header comment in
[`run_perf.sh`](run_perf.sh) (single source of truth).

| File | Purpose |
|------|---------|
| [`run_perf.sh`](run_perf.sh) | One tier; stdout ends with `PERF_METRICS_JSON={...}` |
| [`run_all.sh`](run_all.sh) | Several tiers on one controller; combined summary table |

## Manual run (existing cluster)

Run on a node where `spur` can reach spurctld (`SPUR_CONTROLLER_ADDR`).

```bash
cd tests/native_host/perf/scripts
chmod +x run_perf.sh run_all.sh

# One tier: N jobs, sleep seconds, parallel submitters
./run_perf.sh 1000 0 48

# Multi-tier sweep (warm controller between tiers — see run_all.sh header)
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
variables, see [`../README.md`](../README.md).
