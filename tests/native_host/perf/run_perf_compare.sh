#!/usr/bin/env bash
#
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# run_perf_compare.sh — run the native-host perf pytest suite twice (candidate vs
# baseline binaries), write JSON artifacts, and print a comparison report.
#
# Usage:
#   run_perf_compare.sh CANDIDATE_BIN_DIR BASELINE_BIN_DIR [options]
#
# Options:
#   --candidate-remote-dir PATH   SPUR_TEST_REMOTE_BIN_DIR for candidate (required in CI)
#   --baseline-remote-dir PATH    SPUR_TEST_REMOTE_BIN_DIR for baseline
#   --candidate-label LABEL       SPUR_PERF_RUN_LABEL for candidate (default: candidate)
#   --baseline-label LABEL        SPUR_PERF_RUN_LABEL for baseline (default: baseline)
#   --candidate-json PATH         --perf-json output for candidate (default: /tmp/perf-candidate.json)
#   --baseline-json PATH          --perf-json output for baseline (default: /tmp/perf-baseline.json)
#   --threshold PCT               Regression threshold percent (default: SPUR_PERF_COMPARE_THRESHOLD_PCT or 10)
#   --fail-on-regression          Exit non-zero when fail-gated metrics regress
#
# Environment (required): SPUR_TEST_NODES, SPUR_TEST_SSH_USER
# Optional: SPUR_PERF_TIERS, SPUR_PERF_PARALLEL, SPUR_PERF_SLEEP, SPUR_TEST_SSH_KEY,
# SPUR_TEST_SSH_PASSWORD (password auth via sshpass when set), etc.
#
set -euo pipefail

PERF_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTS_DIR="$(cd "$PERF_DIR/../.." && pwd)"
PYTEST_INI="$TESTS_DIR/pytest.ini"
PERF_TEST="$PERF_DIR/test_scheduler_perf.py"
PYTHONPATH="$TESTS_DIR/native_host"

CANDIDATE_BIN=""
BASELINE_BIN=""
CANDIDATE_REMOTE=""
BASELINE_REMOTE=""
CANDIDATE_LABEL="${SPUR_PERF_CANDIDATE_LABEL:-candidate}"
BASELINE_LABEL="${SPUR_PERF_BASELINE_LABEL:-baseline}"
CANDIDATE_JSON="${TMPDIR:-/tmp}/perf-candidate.json"
BASELINE_JSON="${TMPDIR:-/tmp}/perf-baseline.json"
THRESHOLD="${SPUR_PERF_COMPARE_THRESHOLD_PCT:-10}"
FAIL_ON_REGRESSION=false

usage() {
  sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --candidate-remote-dir) CANDIDATE_REMOTE="$2"; shift 2 ;;
    --baseline-remote-dir) BASELINE_REMOTE="$2"; shift 2 ;;
    --candidate-label) CANDIDATE_LABEL="$2"; shift 2 ;;
    --baseline-label) BASELINE_LABEL="$2"; shift 2 ;;
    --candidate-json) CANDIDATE_JSON="$2"; shift 2 ;;
    --baseline-json) BASELINE_JSON="$2"; shift 2 ;;
    --threshold) THRESHOLD="$2"; shift 2 ;;
    --fail-on-regression) FAIL_ON_REGRESSION=true; shift ;;
    --) shift; break ;;
    -*)
      echo "unknown option: $1" >&2
      usage 1
      ;;
    *)
      if [ -z "$CANDIDATE_BIN" ]; then
        CANDIDATE_BIN="$1"
      elif [ -z "$BASELINE_BIN" ]; then
        BASELINE_BIN="$1"
      else
        echo "unexpected argument: $1" >&2
        usage 1
      fi
      shift
      ;;
  esac
done

if [ -z "$CANDIDATE_BIN" ] || [ -z "$BASELINE_BIN" ]; then
  echo "CANDIDATE_BIN_DIR and BASELINE_BIN_DIR are required" >&2
  usage 1
fi

if [ ! -d "$CANDIDATE_BIN" ] || [ ! -d "$BASELINE_BIN" ]; then
  echo "binary directories must exist" >&2
  exit 1
fi

cleanup_cluster() {
  local remote_dir="${1:-}"
  [ -n "${SPUR_TEST_NODES:-}" ] || return 0
  [ -n "${SPUR_TEST_SSH_USER:-}" ] || return 0

  _ssh_node() {
    local node=$1
    local remote_cmd=$2
    if [ -n "${SPUR_TEST_SSH_PASSWORD:-}" ]; then
      sshpass -p "$SPUR_TEST_SSH_PASSWORD" ssh "${SPUR_TEST_SSH_USER}@${node}" "$remote_cmd"
    else
      ssh -o BatchMode=yes "${SPUR_TEST_SSH_USER}@${node}" "$remote_cmd"
    fi
  }

  IFS=',' read -ra NODES <<< "$SPUR_TEST_NODES"
  for node in "${NODES[@]}"; do
    node="${node#"${node%%[![:space:]]*}"}"
    node="${node%"${node##*[![:space:]]}"}"
    [ -n "$node" ] || continue
    _ssh_node "$node" "
      if [ -n '${remote_dir}' ]; then
        pkill -f '${remote_dir}/spurctld' 2>/dev/null || true
        pkill -f '${remote_dir}/spurd' 2>/dev/null || true
        rm -rf '${remote_dir}' 2>/dev/null || true
      fi
      rm -rf /tmp/spur-e2e-* 2>/dev/null || true
    " || true
  done
}

run_perf_suite() {
  local bin_dir=$1
  local remote_dir=$2
  local label=$3
  local json_out=$4

  export SPUR_TEST_BINARIES_DIR="$bin_dir"
  if [ -n "$remote_dir" ]; then
    export SPUR_TEST_REMOTE_BIN_DIR="$remote_dir"
  else
    unset SPUR_TEST_REMOTE_BIN_DIR || true
  fi
  export SPUR_PERF_RUN_LABEL="$label"

  chmod +x "$bin_dir"/*
  pytest -c "$PYTEST_INI" "$PERF_TEST" -v -m perf -s --perf-json="$json_out"
}

echo "######## PERF CANDIDATE ($CANDIDATE_LABEL) ########"
run_perf_suite "$CANDIDATE_BIN" "$CANDIDATE_REMOTE" "$CANDIDATE_LABEL" "$CANDIDATE_JSON"
cleanup_cluster "$CANDIDATE_REMOTE"

echo
echo "######## PERF BASELINE ($BASELINE_LABEL) ########"
run_perf_suite "$BASELINE_BIN" "$BASELINE_REMOTE" "$BASELINE_LABEL" "$BASELINE_JSON"
cleanup_cluster "$BASELINE_REMOTE"

echo
echo "######## PERF COMPARISON ($CANDIDATE_LABEL vs $BASELINE_LABEL) ########"
COMPARE_ARGS=("$CANDIDATE_JSON" "$BASELINE_JSON" "--threshold" "$THRESHOLD")
if [ "$FAIL_ON_REGRESSION" = true ]; then
  COMPARE_ARGS+=(--fail-on-regression)
fi
PYTHONPATH="$PYTHONPATH" python3 -m perf.report "${COMPARE_ARGS[@]}"
