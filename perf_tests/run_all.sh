#!/usr/bin/env bash
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
  acc=$(grep -E '^ACCEPTED=' "$tier_out" | cut -d= -f2)
  sw=$(grep -E '^SUBMIT_WALL_S=' "$tier_out" | cut -d= -f2)
  st=$(grep -E '^SUBMIT_TPUT_JPS=' "$tier_out" | cut -d= -f2)
  rpc=$(grep -E '^SUBMITJOB_RPC_AVG_US=' "$tier_out" | cut -d= -f2)
  dw=$(grep -E '^DRAIN_WALL_S=' "$tier_out" | cut -d= -f2)
  tw=$(grep -E '^TOTAL_WALL_S=' "$tier_out" | cut -d= -f2)
  e2e=$(grep -E '^E2E_TPUT_JPS=' "$tier_out" | cut -d= -f2)
  pk=$(grep -E '^PEAK_IN_QUEUE=' "$tier_out" | cut -d= -f2)
  qw=$(grep -E '^QUEUE_WAIT_S' "$tier_out" | grep -oE '[0-9]+/[0-9]+/[0-9]+/[0-9]+/[0-9]+' | head -1)
  tt=$(grep -E '^TURNAROUND_S' "$tier_out" | grep -oE '[0-9]+/[0-9]+/[0-9]+/[0-9]+/[0-9]+' | head -1)
  qw_p50=$(echo "$qw" | cut -d/ -f2)
  qw_p95=$(echo "$qw" | cut -d/ -f3)
  qw_max=$(echo "$qw" | cut -d/ -f5)
  tt_p50=$(echo "$tt" | cut -d/ -f2)
  tt_p95=$(echo "$tt" | cut -d/ -f3)
  tt_max=$(echo "$tt" | cut -d/ -f5)
  echo "$n,$acc,$sw,$st,$rpc,$dw,$tw,$e2e,$pk,$qw_p50,$qw_p95,$qw_max,$tt_p50,$tt_p95,$tt_max" >> "$COMBINED"
  rm -f "$tier_out"
done

echo
echo "######## COMBINED RESULTS ########"
column -s, -t "$COMBINED"
