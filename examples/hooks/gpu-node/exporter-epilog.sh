#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

#
# Removes per-GPU exporter metadata written at prolog time.

# shellcheck source=/dev/null
source "${SPUR_HOOKS:-/opt/spur/hooks}/common-env.sh"

EXPORT_DIR="${EXPORT_DIR:-/var/run/exporter/}"

mod128_array() {
    local arr_str="$1"
    local arr result
    IFS=',' read -ra arr <<< "$arr_str"
    for i in "${!arr[@]}"; do
        arr[i]=$(( arr[i] % 128 ))
    done
    result=$(IFS=','; echo "${arr[*]}")
    echo "$result"
}

AMD_SPUR_GPUS=$(mod128_array "${SPUR_JOB_GPUS}")

[[ -d ${EXPORT_DIR} ]] || exit 0

GPUS=$(echo "${AMD_SPUR_GPUS}" | tr "," "\n")
for GPUID in ${GPUS}; do
    [[ -n "$GPUID" ]] || continue
    rm -f "${EXPORT_DIR}/${GPUID}"
done

exit 0
