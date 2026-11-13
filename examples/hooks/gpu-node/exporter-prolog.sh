#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

#
# Writes per-GPU JSON metadata for devicemetrics / exporters.
# Spur-compatible port of exporter-prolog.sh.

# shellcheck source=/dev/null
source "${SPUR_HOOKS:-/opt/spur/hooks}/common-env.sh"

EXPORT_DIR="${EXPORT_DIR:-/var/run/exporter/}"

mapfile -t RENDER_IDS < <(gpu_render_ids "${SPUR_JOB_GPUS}")
if (( ${#RENDER_IDS[@]} == 0 )); then
    echo "exporter-prolog: no GPU render IDs for job ${SPUR_JOB_ID} (SPUR_JOB_GPUS='${SPUR_JOB_GPUS}'), skipping" >&2
    exit 0
fi

mkdir -p "${EXPORT_DIR}"

# One file per GPU, naming only that GPU: deleting a file clears only the GPU
# it is named after.
for id in "${RENDER_IDS[@]}"; do
    cat >"${EXPORT_DIR}/${id}" <<EOF
{
    "SPUR_JOB_ID": "${SPUR_JOB_ID}",
    "SLURM_JOB_ID": "${SLURM_JOB_ID}",
    "SPUR_JOB_USER": "${SPUR_JOB_USER}",
    "SLURM_JOB_USER": "${SLURM_JOB_USER}",
    "SPUR_JOB_PARTITION": "${SPUR_JOB_PARTITION}",
    "SLURM_JOB_PARTITION": "${SLURM_JOB_PARTITION}",
    "SPUR_CLUSTER_NAME": "${SPUR_CLUSTER_NAME}",
    "SLURM_CLUSTER_NAME": "${SLURM_CLUSTER_NAME}",
    "SPUR_JOB_GPUS": "${id}",
    "SLURM_JOB_GPUS": "${id}",
    "CUDA_VISIBLE_DEVICES": "${id}",
    "ROCR_VISIBLE_DEVICES": "${id}",
    "SPUR_SCRIPT_CONTEXT": "${SPUR_SCRIPT_CONTEXT}",
    "SLURM_SCRIPT_CONTEXT": "${SLURM_SCRIPT_CONTEXT}"
}
EOF
done

exit 0
