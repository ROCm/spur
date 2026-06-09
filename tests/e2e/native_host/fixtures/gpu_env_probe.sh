#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
# Lightweight GPU env/isolation probe for spur-devices E2E tests.
set -e

echo "ROCR_VISIBLE_DEVICES=${ROCR_VISIBLE_DEVICES:-}"
echo "SPUR_JOB_GPUS=${SPUR_JOB_GPUS:-}"

count_csv() {
  local val="${1:-}"
  if [ -z "$val" ]; then
    echo 0
    return
  fi
  local n=0
  IFS=',' read -ra _parts <<< "$val"
  for _p in "${_parts[@]}"; do
    [ -n "$_p" ] && n=$((n + 1))
  done
  echo "$n"
}

ROCR_COUNT=$(count_csv "${ROCR_VISIBLE_DEVICES:-}")
SPUR_COUNT=$(count_csv "${SPUR_JOB_GPUS:-}")
echo "VISIBLE_COUNT=$ROCR_COUNT"
echo "SPUR_COUNT=$SPUR_COUNT"

SMI_COUNT=0
if command -v amd-smi >/dev/null 2>&1; then
  SMI_COUNT=$(amd-smi list 2>/dev/null | grep -cE 'GPU|Device' || true)
elif command -v rocm-smi >/dev/null 2>&1; then
  SMI_COUNT=$(rocm-smi --showid 2>/dev/null | grep -cE 'GPU\[|Device Name' || true)
  if [ "$SMI_COUNT" -eq 0 ]; then
    SMI_COUNT=$(rocm-smi --showid 2>/dev/null | grep -c '^[0-9]' || true)
  fi
fi
echo "SMI_COUNT=$SMI_COUNT"

RENDER_COUNT=0
for _d in /dev/dri/renderD*; do
  [ -e "$_d" ] || continue
  RENDER_COUNT=$((RENDER_COUNT + 1))
done
echo "RENDER_COUNT=$RENDER_COUNT"
if [ -e /dev/kfd ]; then
  echo "KFD=yes"
else
  echo "KFD=no"
fi

echo "PROBE_OK"
