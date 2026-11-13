#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Labels each GPU of the job for the AMD Device Metrics Exporter: one file per
# GPU, named by its render ID and naming only that GPU, because the exporter
# clears only the GPU a file is named after when the file is deleted.

EXPORT_DIR="/var/run/exporter"
[ -d "${EXPORT_DIR}" ] && [ -n "${SPUR_JOB_GPUS}" ] || exit 0

# Spur builds the ids in SPUR_JOB_GPUS from each GPU's KFD topology entry.
# Spur 0.13-0.15 and 0.16+ number GPU partitions differently; match both.
declare -A RENDER_ID
for props in /sys/class/kfd/kfd/topology/nodes/*/properties; do
    [ -r "${props}" ] || continue
    minor=0 domain=0 loc=0
    while read -r key value; do
        case "${key}" in
            drm_render_minor) minor=${value} ;;
            domain) domain=${value} ;;
            location_id) loc=${value} ;;
        esac
    done < "${props}"
    (( minor > 0 )) || continue
    RENDER_ID[$(( (domain << 24) | (loc << 8) ))]=${minor}
    RENDER_ID[$(( (domain << 24) | ((loc & ~7) << 8) | (loc & 7) ))]=${minor}
done

IFS=',' read -ra GPU_IDS <<< "${SPUR_JOB_GPUS}"
for gpu in "${GPU_IDS[@]}"; do
    [ -n "${gpu}" ] && [ -n "${RENDER_ID[${gpu}]:-}" ] || continue
    id=${RENDER_ID[${gpu}]}
    printf '{"SLURM_JOB_ID":"%s","SLURM_JOB_USER":"%s","SLURM_JOB_PARTITION":"%s","CUDA_VISIBLE_DEVICES":"%s"}' \
        "${SLURM_JOB_ID}" "${SPUR_JOB_USER}" "${SLURM_JOB_PARTITION}" "${id}" \
        > "${EXPORT_DIR}/${id}"
done
