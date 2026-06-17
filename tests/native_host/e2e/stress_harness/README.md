# stress_harness

Python helpers used by the **native-host** scheduler stress test: parallel `sbatch`,
wait until the active queue is empty, throughput, and latency percentiles from
`scontrol show job` timestamps.

## How it runs (controller node)

The E2E cluster fixture drives the **first node** (controller) over SSH (Paramiko).

For each stress **tier**, the harness:

1. **Uploads** small bash scripts into the cluster’s remote work directory. The script
   bodies live in `harness.py` (they are not checked in as separate files next to this
   README). Names on the controller include `stress_remote_submit.sh`,
   `stress_submit_one.sh`, `stress_remote_latency.sh`, and `stress_latency_sample_one.sh`,
   plus a per-tier batch script and ID file.
2. **Submits** jobs with one remote `bash …/stress_remote_submit.sh` invocation. That
   driver uses `xargs -P` so many `sbatch` calls run in parallel **on the controller**,
   instead of opening one SSH channel per job from the test runner.
3. **Drains** by polling `squeue` from the test process until no active rows remain
   (subject to `SPUR_STRESS_DRAIN_TIMEOUT`).
4. **Samples latency** with a second remote `bash …/stress_remote_latency.sh`, again
   using `xargs -P` over `scontrol show job` for selected job IDs.

So Paramiko sees **a few exec sessions per tier**, not one per `sbatch` or `scontrol`.

The entry test is `tests/native_host/e2e/test_scheduler_stress.py` (pytest marker
`stress`). For the same metrics from shell only, see repo-root `stress_tests/`
(`run_stress.sh`, `run_all.sh`).

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SPUR_STRESS_TIERS` | `50` | Space- or comma-separated job counts per run (e.g. `100 500 1000`). |
| `SPUR_STRESS_PARALLEL` | `32` | Parallel workers on the controller for submit (`xargs -P`, capped at tier size). |
| `SPUR_STRESS_SLEEP` | `0` | Seconds each job sleeps before exit (`0` = exit immediately). |
| `SPUR_STRESS_DRAIN_TIMEOUT` | `1200` | Max seconds to wait for the queue to empty after submit. |
| `SPUR_STRESS_SAMPLE_MAX` | `100` | Max jobs sampled for latency stats (stride over job IDs). |
| `SPUR_STRESS_REMOTE_SCONTROL_PARALLEL` | `16` | Parallel `scontrol show job` workers on the controller during the latency phase. |

Other variables (`SPUR_TEST_NODES`, SSH user/password, binary paths, ports, etc.)
are the same as the rest of the native-host E2E suite; see `docs/developer/building.rst`.

## How to run

```bash
# Default tier (50 jobs)
pytest tests/native_host/e2e/test_scheduler_stress.py -v -m stress

# Larger sweep
SPUR_STRESS_TIERS="100 500 1000" SPUR_STRESS_PARALLEL=48 \
  pytest tests/native_host/e2e/test_scheduler_stress.py -v -m stress
```

The bare-metal E2E GitHub Action runs `pytest …/native_host/e2e/ -m "not stress"`, so
stress is **not** part of that workflow today; run it explicitly when you need it.

## Console summary after a successful run

When the stress test passes, it **prints a markdown-formatted report to stdout**
(controller URL, node list, optional `sinfo` block, throughput table by tier,
latency tables with p50 / p95 / max, short bullets). That is meant for humans
(copy/paste, CI logs, or redirect to a file).

Pytest normally **captures** stdout on passing tests. To always see the report in
your terminal, add **`-s`** (disable capture), for example:

```bash
pytest tests/native_host/e2e/test_scheduler_stress.py -v -m stress -s
```

If capture stays on, you may only see this output when pytest prints it for a
failure, depending on version and settings.
