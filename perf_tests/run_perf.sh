#!/usr/bin/env bash
#
# run_perf.sh — Spur scheduling and ingestion perf benchmark.
#
# Runs ON a cluster node where the `spur` CLI is installed.
#
#   ./run_perf.sh <N> [SLEEP] [PAR]
#     N      number of single-threaded jobs to submit        (default 100)
#     SLEEP  seconds each job sleeps (0 = instant exit)       (default 0)
#     PAR    number of parallel submitter processes           (default 32)
#
# Metrics 1–2 use held submit (-H) to isolate ingestion from scheduling.
# Jobs are released before drain; metric 3 samples the released run. Active jobs
# matching PERF_JOB_NAME are cancelled on exit (success or failure).
#
# Output is printed to stdout only (KEY=VALUE metrics block at end).
#
# Requires GNU coreutils (date -d, sort -g).
#
# Environment: SPUR_CONTROLLER_ADDR (spurctld gRPC, default http://localhost:6817),
# SPUR_CLI, PERF_JOB_NAME, DRAIN_TIMEOUT, SAMPLE_MAX.
#
# ── Timeline (CLIENT = this script / spur CLI; SERVER = spurctld) ─────────────
#
#   CLIENT (cluster node)                 │ SERVER (spurctld)
#   ──────────────────────────────────────┼────────────────────────────────────
#   spur sdiag --reset ──────────────────►│ ResetDiagStats; RpcStats → 0
#   spur sdiag (PRE) ◄────────────────────│ PRE_COUNT, PRE_TOTAL (SubmitJob)
#   t_sub_start ═══════════════════════════════════════════════════════════════
#     PAR × spur submit -H script ───────►│ SubmitJob → Raft commit → Held
#     ◄───────────────────────────────────│ job_id; RpcStats += handler time
#   t_sub_end ══════════════════════════════════════════════════════════════════
#   spur sdiag (POST) ◄───────────────────│ POST_COUNT, POST_TOTAL
#   t_release_start ════════════════════════════════════════════════════════════
#     PAR × spur control release id ─────►│ hold cleared; job schedulable
#   t_release_end ══════════════════════════════════════════════════════════════
#     poll spur jobs (1s); peak depth     │ dispatch → StartTime; run → EndTime
#   t_all_end ══════════════════════════════════════════════════════════════════
#     spur show job (sampled) ◄───────────│ SubmitTime, StartTime, EndTime
#   spur cancel -n PERF_JOB_NAME ────────►│ cancel any still-active tier jobs
#
# Client wall-clock intervals (now = date +%s.%N):
#   SUBMIT_WALL_S  = t_sub_end   − t_sub_start
#   RELEASE_WALL_S = t_release_end − t_release_start
#   DRAIN_WALL_S   = t_all_end   − t_release_end
#   TOTAL_WALL_S   = t_all_end   − t_sub_start
#   (gap t_sub_end → t_release_start is POST-RPC read only; not reported)
#
# ── Metric formulas (stdout KEY=VALUE block) ───────────────────────────────────
#
# Config (inputs, not computed):
#   TIER_N, JOB_SLEEP_S, SUBMITTERS, PERF_JOB_NAME
#
# Acceptance:
#   ACCEPTED = count of job IDs returned by held submit (ids.txt lines)
#
# Metric 1 — submit throughput (client, held submit window only):
#   SUBMIT_WALL_S   = t_sub_end − t_sub_start
#   SUBMIT_TPUT_JPS = ACCEPTED / SUBMIT_WALL_S  (0 if wall is 0)
#
# Metric 2 — SubmitJob RPC handle time (server, sdiag since reset):
#   SUBMITJOB_RPC_COUNT_DELTA    = POST_COUNT − PRE_COUNT
#   SUBMITJOB_RPC_TOTAL_US_DELTA = POST_TOTAL − PRE_TOTAL
#   SUBMITJOB_RPC_AVG_US         = DELTA_TOTAL / DELTA_COUNT  (0 if count is 0)
#
# Release / drain / end-to-end (client wall clock):
#   RELEASE_WALL_S = t_release_end − t_release_start
#   DRAIN_WALL_S   = t_all_end − t_release_end
#   TOTAL_WALL_S   = t_all_end − t_sub_start
#   E2E_TPUT_JPS   = ACCEPTED / TOTAL_WALL_S  (0 if total is 0)
#   PEAK_IN_QUEUE  = max user job count seen while polling spur jobs during drain
#
# Metric 3 — latency percentiles (server timestamps, ~1s resolution):
#   Per sampled job j (every stride-th ID, stride = max(1, ACCEPTED/SAMPLE_MAX)):
#     queue_wait_j  = epoch(StartTime_j) − epoch(SubmitTime_j)
#     run_time_j    = epoch(EndTime_j)   − epoch(StartTime_j)
#     turnaround_j  = epoch(EndTime_j)   − epoch(SubmitTime_j)
#   SAMPLED / COMPLETED_SAMPLED / NONCOMPLETED_SAMPLED from show-job JobState
#   QUEUE_WAIT_S / RUN_TIME_S / TURNAROUND_S min/p50/p95/p99/max:
#     percentiles over sampled jobs; rank p uses index ⌊(n×p+99)/100⌋ on sorted values

set -euo pipefail

export SPUR_CONTROLLER_ADDR="${SPUR_CONTROLLER_ADDR:-http://localhost:6817}"
SPUR_CLI="${SPUR_CLI:-$HOME/spur/bin/spur}"

N="${1:-100}"
SLEEP="${2:-${SLEEP:-0}}"
PAR="${3:-${PAR:-32}}"
DRAIN_TIMEOUT="${DRAIN_TIMEOUT:-1200}"
SAMPLE_MAX="${SAMPLE_MAX:-100}"
JOB_NAME="${PERF_JOB_NAME:-spur_perf_$$}"

TMPDIR="$(mktemp -d)"

cleanup_jobs() {
  "$SPUR_CLI" cancel -n "$JOB_NAME" -Q 2>/dev/null || true
}

trap 'cleanup_jobs; rm -rf "$TMPDIR"' EXIT

die() { echo "ERROR: $*" >&2; exit 1; }

now() { date +%s.%N; }
iso2e() { date -d "$1" +%s 2>/dev/null || echo ""; }

parse_submit_job_rpc_stats() {
  awk '
    /SubmitJob/ && /count:/ {
      count = 0
      total = 0
      for (i = 1; i <= NF; i++) {
        if ($i == "count:" && (i + 1) <= NF) {
          count = $(i + 1) + 0
        }
        if ($i ~ /^total_time_us:/) {
          v = $i
          sub(/^total_time_us:/, "", v)
          total = v + 0
        }
      }
      print count, total
      found = 1
      exit
    }
    END {
      if (!found) print "0 0"
    }
  '
}

fetch_submit_job_rpc_stats() {
  local out
  out="$("$SPUR_CLI" sdiag 2>/dev/null)" || die "spur sdiag failed (is SPUR_CONTROLLER_ADDR reachable?)"
  echo "$out" | parse_submit_job_rpc_stats
}

[ -x "$SPUR_CLI" ] || command -v "$SPUR_CLI" >/dev/null 2>&1 || die "SPUR_CLI not found: $SPUR_CLI"
command -v xargs >/dev/null 2>&1 || die "missing required command: xargs"

echo "================ Spur perf tier ================"
echo "controller : $SPUR_CONTROLLER_ADDR"
echo "spur CLI   : $SPUR_CLI"
echo "N=$N  SLEEP=${SLEEP}s  PAR=$PAR  job_name=$JOB_NAME"

JS="$TMPDIR/job.sh"
{
  echo '#!/usr/bin/env bash'
  echo "#SBATCH --job-name=${JOB_NAME}"
  echo '#SBATCH -N 1'
  echo '#SBATCH -n 1'
  [ "$SLEEP" -gt 0 ] && echo "sleep $SLEEP"
  echo 'exit 0'
} > "$JS"

echo "==> Resetting diagnostic stats (ResetDiagStats)..."
"$SPUR_CLI" sdiag --reset >/dev/null

echo "==> Capturing PRE SubmitJob RPC stats..."
read -r PRE_COUNT PRE_TOTAL <<<"$(fetch_submit_job_rpc_stats)"
echo "    PRE  count=$PRE_COUNT  total_time_us=$PRE_TOTAL"

IDS="$TMPDIR/ids.txt"
: > "$IDS"

echo "==> Submitting $N held jobs (-H) with $PAR parallel submitters (metrics 1–2)..."
t_sub_start="$(now)"
export SPUR_CLI
seq 1 "$N" | xargs -P "$PAR" -I{} bash -c \
  '$SPUR_CLI submit -H "$1" 2>/dev/null | grep -oE "[0-9]+" | head -1' _ "$JS" \
  >> "$IDS" || true
t_sub_end="$(now)"

NSUB="$(grep -c . "$IDS" 2>/dev/null || echo 0)"
SUBMIT_WALL="$(awk "BEGIN{printf \"%.3f\", $t_sub_end - $t_sub_start}")"
SUBMIT_TPUT="$(awk -v wall="$SUBMIT_WALL" -v nsub="$NSUB" 'BEGIN{ if (wall > 0) printf "%.1f", nsub / wall; else print 0 }')"
echo "    accepted=$NSUB  submit_wall=${SUBMIT_WALL}s  submit_throughput=${SUBMIT_TPUT} jobs/s"

echo "==> Capturing POST SubmitJob RPC stats..."
read -r POST_COUNT POST_TOTAL <<<"$(fetch_submit_job_rpc_stats)"
DELTA_COUNT=$((POST_COUNT - PRE_COUNT))
DELTA_TOTAL=$((POST_TOTAL - PRE_TOTAL))
if [ "$DELTA_COUNT" -gt 0 ]; then
  SUBMITJOB_RPC_AVG_US="$(awk "BEGIN{printf \"%.0f\", $DELTA_TOTAL / $DELTA_COUNT}")"
else
  SUBMITJOB_RPC_AVG_US="0"
fi
echo "    POST count=$POST_COUNT  total_time_us=$POST_TOTAL"
echo "    delta  count=$DELTA_COUNT  total_time_us=$DELTA_TOTAL  submitjob_rpc_avg_us=$SUBMITJOB_RPC_AVG_US"

echo "==> Releasing $NSUB held jobs for drain and metric 3..."
t_release_start="$(now)"
export SPUR_CLI
while read -r jid; do
  [ -z "$jid" ] && continue
  echo "$jid"
done < "$IDS" | xargs -r -P "$PAR" -I{} bash -c \
  '$SPUR_CLI control release "$1" 2>/dev/null || true' _ {}
t_release_end="$(now)"
RELEASE_WALL="$(awk "BEGIN{printf \"%.3f\", $t_release_end - $t_release_start}")"
echo "    release_wall=${RELEASE_WALL}s"

echo "==> Waiting for queue to drain (timeout ${DRAIN_TIMEOUT}s)..."
t_drain_deadline=$(( $(date +%s) + DRAIN_TIMEOUT ))
peak_running=0
while :; do
  q="$("$SPUR_CLI" jobs 2>/dev/null | awk 'NR > 1 && $4 == "'"$USER"'"' | wc -l | tr -d ' ')"
  [ "$q" -gt "$peak_running" ] && peak_running="$q"
  [ "$q" -eq 0 ] && break
  [ "$(date +%s)" -ge "$t_drain_deadline" ] && {
    echo "    DRAIN TIMEOUT, $q still queued"
    break
  }
  sleep 1
done
t_all_end="$(now)"
DRAIN_WALL="$(awk "BEGIN{printf \"%.3f\", $t_all_end - $t_release_end}")"
TOTAL_WALL="$(awk "BEGIN{printf \"%.3f\", $t_all_end - $t_sub_start}")"
E2E_TPUT="$(awk -v total="$TOTAL_WALL" -v nsub="$NSUB" 'BEGIN{ if (total > 0) printf "%.1f", nsub / total; else print 0 }')"
echo "    drain_wall=${DRAIN_WALL}s  total_wall=${TOTAL_WALL}s  e2e_throughput=${E2E_TPUT} jobs/s  peak_in_queue=${peak_running}"

echo "==> Sampling job latencies (up to $SAMPLE_MAX jobs)..."
LATENCY_ROWS="$TMPDIR/latency.tsv"
: > "$LATENCY_ROWS"
stride=$(( NSUB / SAMPLE_MAX ))
[ "$stride" -lt 1 ] && stride=1
i=0
completed=0
failed=0
while read -r jid; do
  [ -z "$jid" ] && continue
  i=$((i + 1))
  [ $((i % stride)) -ne 0 ] && continue
  d="$("$SPUR_CLI" show job "$jid" 2>/dev/null || true)"
  st="$(echo "$d" | grep -oE 'JobState=[A-Z_]+' | head -1 | cut -d= -f2)"
  [ "$st" = "COMPLETED" ] && completed=$((completed + 1)) || failed=$((failed + 1))
  sub="$(echo "$d" | grep -oE 'SubmitTime=[0-9T:-]+' | cut -d= -f2)"
  sta="$(echo "$d" | grep -oE 'StartTime=[0-9T:-]+' | cut -d= -f2)"
  end="$(echo "$d" | grep -oE 'EndTime=[0-9T:-]+' | cut -d= -f2)"
  es="$(iso2e "$sub")"
  et="$(iso2e "$sta")"
  ee="$(iso2e "$end")"
  [ -z "$es" ] || [ -z "$et" ] || [ -z "$ee" ] && continue
  echo -e "$((et - es))\t$((ee - et))\t$((ee - es))" >> "$LATENCY_ROWS"
done < "$IDS"

pctl() {
  local col="$1"
  awk -F '\t' '{print $'"$col"'}' "$LATENCY_ROWS" | sort -g > "$TMPDIR/.s"
  local n mn mx p50 p95 p99
  n=$(wc -l < "$TMPDIR/.s" | tr -d ' ')
  [ "$n" -eq 0 ] && { echo "0 0 0 0 0"; return; }
  mn=$(head -1 "$TMPDIR/.s")
  mx=$(tail -1 "$TMPDIR/.s")
  p50=$(sed -n "$(( (n * 50 + 99) / 100 ))p" "$TMPDIR/.s")
  p95=$(sed -n "$(( (n * 95 + 99) / 100 ))p" "$TMPDIR/.s")
  p99=$(sed -n "$(( (n * 99 + 99) / 100 ))p" "$TMPDIR/.s")
  echo "$mn $p50 $p95 $p99 $mx"
}

read -r QW_MIN QW_P50 QW_P95 QW_P99 QW_MAX <<<"$(pctl 1)"
read -r RT_MIN RT_P50 RT_P95 RT_P99 RT_MAX <<<"$(pctl 2)"
read -r TT_MIN TT_P50 TT_P95 TT_P99 TT_MAX <<<"$(pctl 3)"
SAMPLED=$(wc -l < "$LATENCY_ROWS" | tr -d ' ')

{
  echo "TIER_N=$N"
  echo "JOB_SLEEP_S=$SLEEP"
  echo "SUBMITTERS=$PAR"
  echo "ACCEPTED=$NSUB"
  echo "SUBMIT_WALL_S=$SUBMIT_WALL"
  echo "SUBMIT_TPUT_JPS=$SUBMIT_TPUT"
  echo "SUBMITJOB_RPC_COUNT_DELTA=$DELTA_COUNT"
  echo "SUBMITJOB_RPC_TOTAL_US_DELTA=$DELTA_TOTAL"
  echo "SUBMITJOB_RPC_AVG_US=$SUBMITJOB_RPC_AVG_US"
  echo "PERF_JOB_NAME=$JOB_NAME"
  echo "RELEASE_WALL_S=$RELEASE_WALL"
  echo "DRAIN_WALL_S=$DRAIN_WALL"
  echo "TOTAL_WALL_S=$TOTAL_WALL"
  echo "E2E_TPUT_JPS=$E2E_TPUT"
  echo "PEAK_IN_QUEUE=$peak_running"
  echo "SAMPLED=$SAMPLED COMPLETED_SAMPLED=$completed NONCOMPLETED_SAMPLED=$failed"
  echo "QUEUE_WAIT_S min/p50/p95/p99/max=$QW_MIN/$QW_P50/$QW_P95/$QW_P99/$QW_MAX"
  echo "RUN_TIME_S   min/p50/p95/p99/max=$RT_MIN/$RT_P50/$RT_P95/$RT_P99/$RT_MAX"
  echo "TURNAROUND_S min/p50/p95/p99/max=$TT_MIN/$TT_P50/$TT_P95/$TT_P99/$TT_MAX"
}

echo "==> Cleaning up active jobs (name=$JOB_NAME)..."
cleanup_jobs
trap - EXIT
rm -rf "$TMPDIR"
