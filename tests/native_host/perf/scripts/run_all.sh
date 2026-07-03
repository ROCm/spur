#!/usr/bin/env bash
#
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# run_all.sh — drive run_perf.sh across several tiers and aggregate metrics.
#
# Output is printed to stdout only (combined CSV-style table at end).
#
#   ./run_all.sh                 # default tiers: 100 500 1000 2000 (instant jobs)
#   TIERS="100 1000" ./run_all.sh
#   SLEEP=2 ./run_all.sh         # jobs that sleep 2s (forces concurrency)
#
# Environment: SPUR_CONTROLLER_ADDR (default http://localhost:6817), SPUR_CLI,
# TIERS, SLEEP, PAR (passed through to each run_perf.sh tier).
#
# Combined table uses `column` when installed; prints raw CSV otherwise.
# Each tier uses a unique PERF_JOB_NAME on one shared controller; later tiers
# see warm controller state. For isolated tiers per size, use the pytest harness
# (redeploys between tiers) or run one tier per ./run_perf.sh invocation.
#
set -euo pipefail
cd "$(dirname "$0")"

export SPUR_CONTROLLER_ADDR="${SPUR_CONTROLLER_ADDR:-http://localhost:6817}"
export SPUR_CLI="${SPUR_CLI:-$HOME/spur/bin/spur}"
TIERS="${TIERS:-100 500 1000 2000}"
SLEEP="${SLEEP:-0}"
PAR="${PAR:-32}"
STAMP="$(date +%Y%m%d_%H%M%S)"

COMBINED="$(mktemp)"
trap 'rm -f "$COMBINED"' EXIT

_json_field() {
  python3 -c 'import json,sys; d=json.loads(sys.argv[1]); print(d[sys.argv[2]])' "$1" "$2"
}

_json_pct() {
  python3 -c 'import json,sys; d=json.loads(sys.argv[1]); print(d[sys.argv[2]][sys.argv[3]])' "$1" "$2" "$3"
}

echo "tier_n,accepted,submit_wall_s,submit_tput_jps,submitjob_rpc_avg_us,drain_wall_s,total_wall_s,e2e_tput_jps,peak_in_queue,qw_p50,qw_p95,qw_max,tt_p50,tt_p95,tt_max" > "$COMBINED"

echo "######## Spur perf run $STAMP ########"
echo "controller: $SPUR_CONTROLLER_ADDR"
echo "spur CLI  : $SPUR_CLI"
echo "tiers: $TIERS   sleep=${SLEEP}s   submitters=$PAR"

for n in $TIERS; do
  echo
  echo "======================== TIER N=$n ========================"
  tier_out="$(mktemp)"
  PERF_JOB_NAME="spur_perf_${STAMP}_n${n}" ./run_perf.sh "$n" "$SLEEP" "$PAR" 2>&1 | tee "$tier_out"
  json_line=$(grep '^PERF_METRICS_JSON=' "$tier_out" | tail -1)
  if [ -z "$json_line" ]; then
    echo "ERROR: tier N=$n produced no PERF_METRICS_JSON line" >&2
    exit 1
  fi
  payload="${json_line#PERF_METRICS_JSON=}"
  acc=$(_json_field "$payload" accepted)
  sw=$(_json_field "$payload" submit_wall_s)
  st=$(_json_field "$payload" submit_tput_jps)
  rpc=$(_json_field "$payload" submitjob_rpc_avg_us)
  dw=$(_json_field "$payload" drain_wall_s)
  tw=$(_json_field "$payload" total_wall_s)
  e2e=$(_json_field "$payload" e2e_tput_jps)
  pk=$(_json_field "$payload" peak_in_queue)
  qw_p50=$(_json_pct "$payload" queue_wait p50)
  qw_p95=$(_json_pct "$payload" queue_wait p95)
  qw_max=$(_json_pct "$payload" queue_wait max)
  tt_p50=$(_json_pct "$payload" turnaround p50)
  tt_p95=$(_json_pct "$payload" turnaround p95)
  tt_max=$(_json_pct "$payload" turnaround max)
  echo "$n,$acc,$sw,$st,$rpc,$dw,$tw,$e2e,$pk,$qw_p50,$qw_p95,$qw_max,$tt_p50,$tt_p95,$tt_max" >> "$COMBINED"
  rm -f "$tier_out"
done

echo
echo "######## COMBINED RESULTS ########"
if command -v column >/dev/null 2>&1; then
  column -s, -t "$COMBINED"
else
  cat "$COMBINED"
fi
