#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

#
# Writes per-GPU JSON metadata for devicemetrics / exporters.
# Spur-compatible port of exporter-prolog.sh.

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

AMDGPU_DEVICES=$(mod128_array "${CUDA_VISIBLE_DEVICES}")
AMD_SPUR_GPUS=$(mod128_array "${SPUR_JOB_GPUS}")

MSG=$(
    cat <<EOF
{
    "SPUR_JOB_ID": "${SPUR_JOB_ID}",
    "SLURM_JOB_ID": "${SLURM_JOB_ID}",
    "SPUR_JOB_USER": "${SPUR_JOB_USER}",
    "SLURM_JOB_USER": "${SLURM_JOB_USER}",
    "SPUR_JOB_PARTITION": "${SPUR_JOB_PARTITION}",
    "SLURM_JOB_PARTITION": "${SLURM_JOB_PARTITION}",
    "SPUR_CLUSTER_NAME": "${SPUR_CLUSTER_NAME}",
    "SLURM_CLUSTER_NAME": "${SLURM_CLUSTER_NAME}",
    "SPUR_JOB_GPUS": "${AMD_SPUR_GPUS}",
    "SLURM_JOB_GPUS": "${AMD_SPUR_GPUS}",
    "CUDA_VISIBLE_DEVICES": "${AMDGPU_DEVICES}",
    "ROCR_VISIBLE_DEVICES": "${AMDGPU_DEVICES}",
    "SPUR_SCRIPT_CONTEXT": "${SPUR_SCRIPT_CONTEXT}",
    "SLURM_SCRIPT_CONTEXT": "${SLURM_SCRIPT_CONTEXT}"
}
EOF
)

mkdir -p "${EXPORT_DIR}"

# Prolog runs before CUDA/ROCR env injection; use allocated device IDs from Spur.
GPUS=$(echo "${AMD_SPUR_GPUS}" | tr "," "\n")
if [[ -z "${AMD_SPUR_GPUS//,/}" ]]; then
    echo "exporter-prolog: SPUR_JOB_GPUS empty for job ${SPUR_JOB_ID}, skipping" >&2
    exit 0
fi

for GPUID in ${GPUS}; do
    [[ -n "$GPUID" ]] || continue
    echo "${MSG}" >"${EXPORT_DIR}/${GPUID}"
done

exit 0
